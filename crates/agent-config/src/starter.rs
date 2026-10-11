//! The file `smoll init` writes, and the text that goes in it.
//!
//! The starter is a working configuration rather than a commented-out one: it
//! runs against the mock provider, so `smoll init && smoll task "..."` does
//! something on a machine with no model downloaded yet. Every other provider is
//! shown commented, because copying a block and uncommenting it is how people
//! actually change a config.

use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::load::PROJECT_FILE;

/// Written verbatim, so the docs and the file cannot drift apart.
pub fn starter_toml() -> &'static str {
    STARTER
}

/// Create the project file. Refuses to overwrite unless `force` is set, because
/// a configuration someone edited by hand is not ours to replace.
pub fn write_starter(cwd: &Path, force: bool) -> Result<PathBuf, ConfigError> {
    let path = cwd.join(PROJECT_FILE);
    if path.exists() && !force {
        return Err(ConfigError::Exists { path });
    }
    std::fs::write(&path, STARTER).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

const STARTER: &str = r#"# smoll configuration
#
# This file is safe to commit: it names environment variables, never secrets.
# A user-wide file at ~/.config/smoll/config.toml is read first and this one
# overrides it key by key; SMOLL_* variables override both.

[agent]
# suggest-only    propose a diff, write nothing, run nothing
# approve-edits   ask before writing a file, run commands freely
# approve-commands ask before running a command, write files freely
# autonomous-safe write and run without asking, but only what the sandbox allows
approval_mode = "approve-edits"
max_steps = 20
# The most prompt a run may plan for. The model's own window still caps it: a
# 4k GGUF gets a 4k prompt whatever is written here.
max_context_tokens = 16000
timeout_seconds = 120
audit_log = true

[ui]
theme = "dark"
color = true
streaming = true
show_tool_logs = true

# The provider that answers. `mock` replies from a script, which is why the
# first run works before any model is downloaded.
[providers.mock]
type = "mock"

# A GGUF file you already have, run on this machine.
# [providers.local]
# type = "gguf"
# path = "~/Models/qwen2.5-coder-1.5b-q4_k_m.gguf"
# context_length = 8192

# Anything that speaks the OpenAI wire format on this machine: Ollama, LM
# Studio, vLLM, TGI, llama-server.
# [providers.ollama]
# type = "openai_compatible"
# base_url = "http://localhost:11434/v1"
# model = "qwen2.5-coder:1.5b"
# A server that needs a key takes the NAME of a variable, never the key itself:
# api_key_env = "OLLAMA_API_KEY"
# Ask only a server you know fills a tool list, or the run sticks to prose:
# tool_calling = true

# Pick one when there is more than one:
# [agent]
# provider = "ollama"

# Per-tool policy, one section per tool. The name in the brackets has to be a
# tool this build actually registers: a section for a name nobody answers gates
# nothing, and `smoll doctor` says when that is so.
# A string in `allowlist` is a prefix a command must start with; `denylist` is
# checked first and wins.
[tools.run_command]
enabled = true
sandbox = "strict"
allowlist = ["cargo test", "cargo clippy", "cargo fmt", "cargo check", "git status", "git diff"]
denylist = ["rm -rf", "sudo", "git push --force", "curl | sh"]

[tools.search_text]
enabled = true

# `delete_file` is the one default tool that removes something. It always asks a
# person, whatever the approval mode, and `smoll rollback` puts the file back
# from what the run recorded. To take it off the table for a project:
# [tools.delete_file]
# enabled = false

[privacy]
telemetry = false
redact_secrets = true
workspace_only = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starter_is_valid_toml() {
        toml::from_str::<toml::Value>(STARTER).expect("the starter must parse");
    }

    #[test]
    fn the_starter_loads_and_needs_no_model_on_disk() {
        // The commented blocks must stay comments: an active gguf provider with
        // a path this machine does not have would make `smoll init` produce a
        // configuration that cannot run.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(PROJECT_FILE), STARTER).expect("write");
        let loaded = crate::load_with(dir.path(), &|_| None).expect("the starter is valid");
        assert_eq!(loaded.config.providers.len(), 1);
        assert_eq!(
            loaded.config.providers["mock"].kind,
            crate::ProviderKind::Mock
        );
    }

    #[test]
    fn writing_refuses_to_clobber_without_force() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = crate::write_starter(dir.path(), false).expect("written");
        assert!(first.exists());
        let error = crate::write_starter(dir.path(), false).expect_err("second attempt fails");
        assert!(error.to_string().contains("already exists"), "{error}");
        crate::write_starter(dir.path(), true).expect("--force replaces it");
    }
}
