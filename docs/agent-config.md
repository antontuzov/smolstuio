# Agent configuration

`smoll` reads three layers, each overriding the one before it:

| Layer | Location | Notes |
| --- | --- | --- |
| user | `$XDG_CONFIG_HOME/smoll/config.toml`, else `~/.config/smoll/config.toml` | `$SMOLL_CONFIG` replaces this path entirely |
| project | `./smoll.toml` | safe to commit; it names environment variables, never secrets |
| environment | `SMOLL_*` | wins over both files |

A missing file is normal. A file that exists and is broken is an error, and the
error names the path and the key:

```console
$ smoll config
error: the configuration does not match what this build expects
  unknown field `max_stepz`, expected one of `name`, `provider`, …
  from: /home/you/project/smoll.toml
```

That refusal is deliberate. A typo in a section name (`[aegnt]`) would otherwise
read as an empty section, silently drop every setting in it, and quietly replace
your approval mode with the default one.

`smoll init` writes a working `smoll.toml` that answers from the mock provider,
so the first run needs no model on disk. It refuses to overwrite an existing
file unless you pass `--force`.

## `[agent]`

```toml
[agent]
approval_mode = "approve-edits"
max_steps = 20
max_context_tokens = 8000
timeout_seconds = 120
audit_log = true
retries = 2
```

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `approval_mode` | `approve-edits` | see below | how much may happen without asking |
| `max_steps` | `20` | 1–200 | tool calls allowed in one task |
| `max_context_tokens` | `8000` | 256–1000000 | budget for the prompt |
| `timeout_seconds` | `120` | 1–3600 | wall-clock limit for one task |
| `audit_log` | `true` | | every action is recorded |
| `retries` | `2` | | attempts after a provider error |

### Approval modes

| Mode | Writes a file | Runs a command |
| --- | --- | --- |
| `suggest-only` | never; the diff is the output | never |
| `approve-edits` | **asks first** | yes |
| `approve-commands` | yes | **asks first** |
| `autonomous-safe` | yes, if the sandbox allows it | yes, if the sandbox allows it |

`approve-edits` is the default: an agent that edits code should ask before it
edits code. Read-only tools — reading a file, listing a directory, `git status`
— never ask in any mode other than `suggest-only` refusing to change anything.

## `[providers.<name>]`

Each provider is a named block, and `agent.provider` picks one. With a single
block defined, the choice is implied.

```toml
[providers.ollama]
type = "openai_compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:1.5b"
context_length = 8192
```

| `type` | Required keys | Runs |
| --- | --- | --- |
| `mock` | none | from a script; used by tests and `smoll init` |
| `echo` | none | repeats the rendered prompt back |
| `openai_compatible` | `base_url` | any OpenAI-format HTTP server: Ollama, LM Studio, vLLM, TGI, llama-server |
| `gguf` | `path` (a `.gguf` file) | this machine, through the engine layer |
| `candle` | `model` | this machine |
| `onnx` | `path` | this machine |
| `huggingface` | `repo_id` or `model` | Hub, downloaded locally or over its inference API |

Optional keys on any provider: `api_key_env`, `token_env`, `device`,
`context_length`, `temperature`, `top_p`, `max_tokens`, `stop`.

Which of these a given binary can actually run is [providers.md](providers.md):
`gguf` answers from its weights only in a build made with `--features
llama-cpp`, and `openai_compatible` is the next adapter to be written. Asking
for one this build does not have is an error naming the missing piece, never a
silent substitution.

`api_key_env` and `token_env` are **environment variable names**, not keys. A
value containing `=` or starting with `sk-` is refused, because the config file
is meant to be safe to commit.

## `[tools.<name>]`

```toml
[tools.shell]
enabled = true
sandbox = "strict"
allowlist = ["cargo test", "cargo clippy", "cargo fmt", "cargo check", "git status", "git diff"]
denylist = ["rm -rf", "sudo", "git push --force", "curl | sh"]
```

`sandbox` is `off`, `warn` (log what would have been blocked) or `strict`
(block it). An allowlist entry is a prefix a command must start with; the
denylist is checked first and wins. A non-empty `allowlist` with no `sandbox`
key is refused — a list nothing enforces is decoration, not policy.

## `[privacy]`

```toml
[privacy]
telemetry = false
redact_secrets = true
workspace_only = true
```

`workspace_only` keeps file operations inside the repository. `redact_secrets`
masks tokens and keys in logs and in tool output. `telemetry` exists for
compatibility with configs copied from other tools: this build contains no
telemetry code, so setting it to `true` changes nothing and `smoll` says so in a
warning rather than silently accepting the claim.

## Environment variables

| Variable | Sets |
| --- | --- |
| `SMOLL_CONFIG` | path to the user configuration file |
| `SMOLL_APPROVAL_MODE` | `agent.approval_mode` |
| `SMOLL_SANDBOX` | `tools.*.sandbox`, for every tool |
| `SMOLL_MAX_STEPS` | `agent.max_steps` |
| `SMOLL_MAX_CONTEXT_TOKENS` | `agent.max_context_tokens` |
| `SMOLL_TIMEOUT_SECONDS` | `agent.timeout_seconds` |
| `SMOLL_PROVIDER` | `agent.provider` |
| `SMOLL_MODEL` | `providers.<name>.model` for the provider in use |
| `SMOLL_WORKSPACE_ONLY` | `privacy.workspace_only` |
| `SMOLL_REDACT_SECRETS` | `privacy.redact_secrets` |

Booleans accept `true`, `false`, `1`, `0`, `yes`, `no`, `on`, `off`. Anything
else is refused with the variable's name, not the file key it corresponds to.

## Checking what you actually got

```console
$ smoll config          # the resolved configuration, as TOML, with the files it came from on stderr
$ smoll config --json   # the same thing for a script
$ smoll doctor          # this machine, this repository and this config, read and reported
$ smoll tools           # what a run may be given, and the prompt tokens that costs on every step
```

Keys are snake_case throughout; the values of `approval_mode` and `sandbox` are
kebab-case. An unknown key at any level is an error rather than a shrug.

## Changing it for one run

`smoll task` takes three flags that override the file for a single run instead of
editing it: `--approval-mode MODE`, `--dry-run` (this run changes nothing; every
write comes back as a proposed diff) and `--yes` (ask nobody, which is what a CI
job needs and the policy still refuses what the policy refuses).

Whatever a run did, it leaves behind `.smoll/sessions/<id>.json`: the transcript,
the token count, and the journal of what each write replaced. `smoll review`
shows one of those records — the newest, unless you give `--id` — and `smoll
rollback` puts the recorded files back the way the run found them. A file you
edited afterwards is named and left alone rather than overwritten again.
