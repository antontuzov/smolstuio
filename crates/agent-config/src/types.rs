//! The resolved shape of a configuration, and the rules it must satisfy.
//!
//! Nothing here reads a file or an environment variable. Loading lives in
//! [`crate::load`]; this module is the target both the parser and the agent
//! loop agree on.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Problem;

/// How much the agent may do without asking.
///
/// Keys are snake_case throughout a configuration; the values of this enum are
/// kebab-case, which is how they are written in `[agent] approval_mode`.
/// A value from the brief's example config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Propose only: no file is written and no command is run.
    SuggestOnly,
    /// File writes need approval; commands run on their own. The default,
    /// because an agent that edits code should ask before it edits code.
    #[default]
    ApproveEdits,
    /// Commands need approval; file writes apply on their own.
    ApproveCommands,
    /// Nothing asks, but only actions the sandbox policy allows are taken.
    AutonomousSafe,
}

impl ApprovalMode {
    pub const ALL: [Self; 4] = [
        Self::SuggestOnly,
        Self::ApproveEdits,
        Self::ApproveCommands,
        Self::AutonomousSafe,
    ];

    /// Writes may happen at all. False only in `suggest-only`, where a proposed
    /// diff is the final product.
    pub fn allows_write(self) -> bool {
        !matches!(self, Self::SuggestOnly)
    }

    /// A write may proceed without a human answering first.
    pub fn auto_writes(self) -> bool {
        matches!(self, Self::ApproveCommands | Self::AutonomousSafe)
    }

    /// A command may proceed without a human answering first.
    pub fn auto_commands(self) -> bool {
        matches!(self, Self::ApproveEdits | Self::AutonomousSafe)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SuggestOnly => "suggest-only",
            Self::ApproveEdits => "approve-edits",
            Self::ApproveCommands => "approve-commands",
            Self::AutonomousSafe => "autonomous-safe",
        }
    }
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown approval mode {s:?}, expected one of {}",
                    list(&Self::ALL.map(|m| m.as_str())[..])
                )
            })
    }
}

/// How strictly a tool's action is checked before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Policy is recorded but never blocks.
    Off,
    /// Policy blocks nothing and logs what it would have blocked.
    Warn,
    /// Policy blocks.
    Strict,
}

impl SandboxMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::Warn, Self::Strict];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Strict => "strict",
        }
    }

    pub fn blocks(self) -> bool {
        matches!(self, Self::Strict)
    }
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown sandbox mode {s:?}, expected one of {}",
                    list(&Self::ALL.map(|m| m.as_str())[..])
                )
            })
    }
}

fn list(items: &[&'static str]) -> String {
    items.join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Scripted replies for tests and for `smoll demo`.
    Mock,
    /// Repeats the prompt back; the way to see the exact rendered context.
    Echo,
    /// Any OpenAI-compatible HTTP server: Ollama, LM Studio, vLLM, TGI, llama-server.
    OpenaiCompatible,
    /// A GGUF file read through the local engine layer.
    Gguf,
    #[serde(rename = "candle")]
    Candle,
    Onnx,
    /// Hugging Face Hub, locally downloaded or served over its inference API.
    Huggingface,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Echo => "echo",
            Self::OpenaiCompatible => "openai_compatible",
            Self::Gguf => "gguf",
            Self::Candle => "candle",
            Self::Onnx => "onnx",
            Self::Huggingface => "huggingface",
        }
    }

    /// Whether the prompts this provider sends stay on the machine. Model
    /// *downloads* are a different question: a local backend still fetches
    /// weights, but it never sends repository content anywhere.
    pub fn keeps_prompts_local(self) -> bool {
        !matches!(self, Self::OpenaiCompatible | Self::Huggingface)
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One named entry under `[providers.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    #[serde(default)]
    pub model: Option<String>,
    /// Hugging Face `repo-id` for `huggingface` and for a local folder's origin.
    #[serde(default)]
    pub repo_id: Option<String>,
    /// Path to a local model file for `gguf` and `onnx`.
    #[serde(default)]
    pub path: Option<String>,
    /// Base URL of an OpenAI-compatible server, with or without `/v1`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// The name of an environment variable holding an API key. The variable
    /// name, never the key itself, is what belongs in a config file.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub inference_endpoint: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Vec<String>,
    /// Whether to trust this server with a tool list. Off unless a config says
    /// yes: a small model that is handed tools it cannot fill answers worse than
    /// one that is asked for a fenced JSON block, and a server that does not
    /// implement `tools` at all would silently ignore the field.
    #[serde(default)]
    pub tool_calling: Option<bool>,
}

impl ProviderConfig {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            model: None,
            repo_id: None,
            path: None,
            base_url: None,
            api_key_env: None,
            token_env: None,
            inference_endpoint: None,
            device: None,
            context_length: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: Vec::new(),
            tool_calling: None,
        }
    }

    fn validate(&self, key: &str, problems: &mut Vec<Problem>) {
        let require = |field: &str, present: &Option<String>, problems: &mut Vec<Problem>| {
            let missing = match present {
                Some(value) => value.trim().is_empty(),
                None => true,
            };
            if missing {
                problems.push(Problem::new(
                    format!("{key}.{field}"),
                    format!("a provider of type {} needs it", self.kind),
                ));
            }
        };

        match self.kind {
            ProviderKind::OpenaiCompatible => require("base_url", &self.base_url, problems),
            ProviderKind::Huggingface => {
                if self.repo_id.is_none() && self.model.is_none() {
                    problems.push(Problem::new(
                        format!("{key}.repo_id"),
                        "a huggingface provider needs repo_id or model",
                    ));
                }
            }
            ProviderKind::Gguf => {
                require("path", &self.path, problems);
                if let Some(path) = &self.path {
                    if !path.to_lowercase().ends_with(".gguf") {
                        problems.push(Problem::new(
                            format!("{key}.path"),
                            format!("{path} is not a .gguf file"),
                        ));
                    }
                }
            }
            ProviderKind::Onnx => require("path", &self.path, problems),
            ProviderKind::Candle => require("model", &self.model, problems),
            ProviderKind::Mock | ProviderKind::Echo => {}
        }

        if let Some(length) = self.context_length {
            if length < 256 {
                problems.push(Problem::new(
                    format!("{key}.context_length"),
                    format!("{length} tokens is too small to hold a prompt and a reply"),
                ));
            }
        }
        // Only two kinds read this. A GGUF file answers from a chat template that
        // this build cannot check, so claiming tool calling for it would let a run
        // send a tool list the model then ignores — the config says so instead.
        if self.tool_calling.is_some()
            && !matches!(
                self.kind,
                ProviderKind::OpenaiCompatible | ProviderKind::Mock
            )
        {
            problems.push(Problem::new(
                format!("{key}.tool_calling"),
                format!("a provider of type {} does not read this", self.kind),
            ));
        }
        for (field, value) in [("temperature", self.temperature), ("top_p", self.top_p)] {
            if let Some(value) = value {
                if !(0.0..=2.0).contains(&value) || (field == "top_p" && value > 1.0) {
                    problems.push(Problem::new(
                        format!("{key}.{field}"),
                        format!("{value} is outside the range this build will send"),
                    ));
                }
            }
        }
        for (field, name) in [
            ("api_key_env", &self.api_key_env),
            ("token_env", &self.token_env),
        ] {
            if let Some(name) = name {
                // A config file names the variable. Someone who pastes the key
                // itself gets told, instead of a secret sitting in a file that
                // is meant to be shareable.
                if name.contains('=')
                    || name.starts_with("sk-")
                    || name.split_whitespace().count() > 1
                {
                    problems.push(Problem::new(
                        format!("{key}.{field}"),
                        "this must be the NAME of an environment variable, not a key",
                    ));
                } else if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    problems.push(Problem::new(
                        format!("{key}.{field}"),
                        format!("{name:?} is not a usable environment variable name"),
                    ));
                }
            }
        }
    }
}

/// Per-tool settings, from `[tools.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub sandbox: Option<SandboxMode>,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

/// Written by hand rather than derived, because the derived default has
/// `enabled: false` — and a tool section a caller constructed to set a timeout
/// or a denylist is a tool they intend to use. This matches what
/// `[tools.read_file]` with no `enabled` key parses to.
impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            sandbox: None,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            timeout_seconds: None,
            max_output_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    #[serde(default = "default_name")]
    pub name: String,
    /// Which entry in `providers` to use. Optional when there is only one.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    /// The boundary for every tool that does not name its own. Absent means
    /// `strict`, which is the only direction a default can safely go.
    #[serde(default)]
    pub sandbox: Option<SandboxMode>,
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub audit_log: bool,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            name: default_name(),
            provider: None,
            approval_mode: ApprovalMode::default(),
            sandbox: None,
            max_steps: default_max_steps(),
            max_context_tokens: default_max_context_tokens(),
            timeout_seconds: default_timeout_seconds(),
            audit_log: true,
            retries: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    #[serde(default = "default_theme")]
    pub theme: Theme,
    #[serde(default = "default_true")]
    pub color: bool,
    #[serde(default = "default_true")]
    pub streaming: bool,
    #[serde(default = "default_true")]
    pub show_tool_logs: bool,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            color: true,
            streaming: true,
            show_tool_logs: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacySection {
    #[serde(default)]
    pub telemetry: bool,
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    #[serde(default = "default_true")]
    pub workspace_only: bool,
}

impl Default for PrivacySection {
    fn default() -> Self {
        Self {
            telemetry: false,
            redact_secrets: true,
            workspace_only: true,
        }
    }
}

/// A fully resolved configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub ui: UiSection,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPolicy>,
    #[serde(default)]
    pub privacy: PrivacySection,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("mock".to_owned(), ProviderConfig::new(ProviderKind::Mock));
        Self {
            agent: AgentSection::default(),
            ui: UiSection::default(),
            providers,
            tools: BTreeMap::new(),
            privacy: PrivacySection::default(),
        }
    }
}

impl Config {
    /// A configuration with no model at all cannot run a loop, so a mock
    /// provider appears when the file names none.
    pub fn ensure_a_provider(&mut self) {
        if self.providers.is_empty() {
            self.providers
                .insert("mock".to_owned(), ProviderConfig::new(ProviderKind::Mock));
        }
    }

    /// The provider the loop should use, and the key it was configured under.
    pub fn resolve_provider(&self) -> Result<(&str, &ProviderConfig), Vec<Problem>> {
        let mut problems = Vec::new();
        if let Some(name) = &self.agent.provider {
            return match self.providers.get(name) {
                Some(provider) => Ok((name, provider)),
                None => {
                    problems.push(Problem::new(
                        "agent.provider",
                        format!(
                            "{name} is not defined; the file defines {}",
                            self.providers
                                .keys()
                                .map(|k| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                    Err(problems)
                }
            };
        }
        if self.providers.len() == 1 {
            let (name, provider) = self.providers.iter().next().expect("len is 1");
            return Ok((name, provider));
        }
        problems.push(Problem::new(
            "agent.provider",
            format!(
                "this configuration defines {} providers and does not choose one: {}",
                self.providers.len(),
                self.providers
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
        Err(problems)
    }

    /// Render back to TOML, which is what `smoll config` prints and what a
    /// saved configuration is written from.
    pub fn to_toml_string(&self) -> Result<String, crate::ConfigError> {
        toml::to_string_pretty(&toml::Value::try_from(self).map_err(|source| {
            crate::ConfigError::Shape {
                message: source.to_string(),
                sources: "the resolved configuration".to_owned(),
            }
        })?)
        .map_err(|source| crate::ConfigError::Shape {
            message: source.to_string(),
            sources: "the resolved configuration".to_owned(),
        })
    }

    /// Everything wrong with this configuration, in one pass.
    pub fn validate(&self) -> Result<Vec<String>, crate::ConfigError> {
        let mut problems = Vec::new();
        let mut warnings = Vec::new();

        if self.agent.max_steps == 0 || self.agent.max_steps > 200 {
            problems.push(Problem::new(
                "agent.max_steps",
                format!(
                    "{} is outside 1..=200; a loop this size is a mistake, not a setting",
                    self.agent.max_steps
                ),
            ));
        }
        if !(256..=1_000_000).contains(&self.agent.max_context_tokens) {
            problems.push(Problem::new(
                "agent.max_context_tokens",
                format!("{} is outside 256..=1000000", self.agent.max_context_tokens),
            ));
        }
        if self.agent.timeout_seconds == 0 || self.agent.timeout_seconds > 3600 {
            problems.push(Problem::new(
                "agent.timeout_seconds",
                format!("{} is outside 1..=3600", self.agent.timeout_seconds),
            ));
        }
        if self.providers.is_empty() {
            problems.push(Problem::new(
                "providers",
                "no provider is defined, so nothing can answer",
            ));
        }
        for (name, provider) in &self.providers {
            provider.validate(&format!("providers.{name}"), &mut problems);
            if provider.kind == ProviderKind::OpenaiCompatible {
                if let Some(url) = &provider.base_url {
                    if !is_loopback(url) {
                        warnings.push(format!(
                            "providers.{name} sends every prompt to {url}, which is not this \
                             machine; your repository context leaves it while that provider runs"
                        ));
                    }
                }
            }
            if provider.kind == ProviderKind::Huggingface && provider.path.is_none() {
                warnings.push(format!(
                    "providers.{name} is a Hugging Face provider without a local path: prompts \
                     are sent to the inference endpoint"
                ));
            }
        }
        for (name, policy) in &self.tools {
            if policy.sandbox.is_none() && !policy.allowlist.is_empty() {
                problems.push(Problem::new(
                    format!("tools.{name}.sandbox"),
                    "an allowlist needs sandbox = \"strict\" or \"warn\" to mean anything",
                ));
            }
            for (list, entries) in [
                ("allowlist", &policy.allowlist),
                ("denylist", &policy.denylist),
            ] {
                if entries.iter().any(|entry| entry.trim().is_empty()) {
                    problems.push(Problem::new(
                        format!("tools.{name}.{list}"),
                        "an empty entry would match every command",
                    ));
                }
            }
        }
        if self.privacy.telemetry {
            warnings.push(
                "privacy.telemetry = true is set but this build contains no telemetry code, so \
                 it changes nothing"
                    .to_owned(),
            );
        }
        if let Err(provider_problems) = self.resolve_provider() {
            problems.extend(provider_problems);
        }

        if problems.is_empty() {
            Ok(warnings)
        } else {
            Err(crate::ConfigError::invalid(problems))
        }
    }
}

/// Whether a URL points at this machine.
///
/// Deliberately narrow: anything this function is unsure about is reported as
/// remote, because the question it answers is "might your repository context
/// leave the machine".
fn is_loopback(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    // A bracketed IPv6 literal contains colons that are not a port separator.
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(bracketed),
        None => authority.split(':').next().unwrap_or(authority),
    };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

fn default_name() -> String {
    "smoll".to_owned()
}

fn default_theme() -> Theme {
    Theme::Dark
}

fn default_true() -> bool {
    true
}

fn default_max_steps() -> usize {
    20
}

fn default_max_context_tokens() -> usize {
    8_000
}

fn default_timeout_seconds() -> u64 {
    120
}

fn default_retries() -> u32 {
    2
}
