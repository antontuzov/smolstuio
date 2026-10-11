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
max_context_tokens = 16000
timeout_seconds = 120
audit_log = true
retries = 2
```

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `approval_mode` | `approve-edits` | see below | how much may happen without asking |
| `sandbox` | `strict` | `off`/`warn`/`strict` | the boundary for a tool whose own section says nothing |
| `max_steps` | `20` | 1–200 | tool calls allowed in one task |
| `max_context_tokens` | `16000` | 256–1000000 | the most prompt a run may plan for |
| `timeout_seconds` | `120` | 1–3600 | wall-clock limit for one task |
| `audit_log` | `true` | | every action is recorded |
| `retries` | `2` | | attempts after a provider error |

`max_context_tokens` is a ceiling, not a promise: a run takes the smaller of this
and the window the provider actually advertises (`[providers.<name>]
context_length`, or what a server reports), so a 4k GGUF still gets a 4k prompt
whatever is written here. What is left after the rules and the tool list goes to
the repository — about a quarter to the repo map, the rest to the file bodies the
first turn is handed — and the map keeps a floor even when the window leaves
nothing to spare, because a model with no table of contents assumes the
repository is the three files it was shown. `smoll tools` prints what offering
the twenty-four default tools costs on every step of a run: 3217 tokens together.

That cost is counted whether a model is shown the tools as a native block or has
them written into its instructions, so the number a run reports is the number the
window has to hold, answer room included. Measured on this machine against a
two-file repository, a first step wants 3743 tokens of a model that is handed the
tools as a native block and 2250 of one that has to be told them in prose — the
prose list is one sentence per tool where the block is a JSON schema per tool, so
for the smallest windows the *less* capable provider is the one that gets room
for file bodies. At 4096 tokens a native-block model gets the rules, the whole
tool list, the plan and the repo map, and no file bodies at all: it has to ask
for the file it wants, which is why `read_file` is the cheapest tool to keep.
Switching eight of them off (`git_status`, `git_diff`, `git_log`, `git_show`,
`patch_file`, `task_plan_update`, `memory_recall`, `check_run`) leaves 16 offered
and 2126 tokens of list. A run that still does not fit after dropping every file
and every line of the map stops with `context tokens needed`, and the hint under
it names the part `smoll tools` can give back.

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
`tool_calling` is read only by `mock` and `openai_compatible`; anywhere else it
is a validation error, because the field would be silently ignored.

Which of these a given binary can actually run is [providers.md](providers.md):
`gguf` answers from its weights only in a build made with `--features
llama-cpp`, and `openai_compatible` needs `--features openai-compatible`. Asking
for one this build does not have is an error naming the missing piece, never a
silent substitution.

`api_key_env` and `token_env` are **environment variable names**, not keys. A
value containing `=` or starting with `sk-` is refused, because the config file
is meant to be safe to commit.

## `[tools.<name>]`

```toml
[tools.run_command]
enabled = true
sandbox = "strict"
allowlist = ["cargo test", "cargo clippy", "cargo fmt", "cargo check", "git status", "git diff"]
denylist = ["rm -rf", "sudo", "git push --force", "curl | sh"]
```

`sandbox` is `off`, `warn` (log what would have been blocked) or `strict`
(block it). An allowlist entry is a prefix a command must start with; the
denylist is checked first and wins. A non-empty `allowlist` with no `sandbox`
key is refused — a list nothing enforces is decoration, not policy.

The five tools that check the project's own work (`test_run`, `check_run`,
`lint_run`, `format_run`, `build_run`) read their command out of the repository's manifests
instead of taking arguments from the model, so there is no command line for a
person to approve in advance: what they can be given is a `denylist` pattern over
the command the project declares, a lower `timeout_seconds`, and `enabled =
false` to keep the tool out of the prompt altogether. `format_run` goes one step
further and only asks the formatter to report which files it would change; a
formatter it cannot put in report mode — a `npm run fmt` script, a `make fmt`
target, anything carrying `--fix` or `--write` — is refused rather than run.

**The name in the brackets has to be a tool this build registers.** A section
for any other name parses, applies to nothing, and leaves a denylist that looks
enforced sitting in a file doing nothing, which is the worse way to believe your
`rm -rf` is barred. `smoll tools` lists the names that mean something, and both
`smoll tools` and `smoll doctor` warn about a section that matches none:

```console
$ smoll doctor
warning: [tools.shell] gates nothing: no tool this build registers has that
         name. Check the spelling against `smoll tools`, which lists what a run
         can be given.
```

This is checked in the CLI rather than in the parser, because the parser does not
know the tool list and must not have to.

For what each of the twenty-four tools actually is — its permission class, where
its arguments come from, what it may touch, and how many prompt tokens offering
it costs on every step of a run — see [tools.md](tools.md). This file is only
about how a run is configured.

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
| `SMOLL_SANDBOX` | `agent.sandbox`, the boundary for every tool that does not name its own |
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

A file `delete_file` removed is put back from the same journal, which is why that
tool refuses a binary or an over-large file instead of removing it: the journal
holds text, and a deletion it cannot undo is a deletion that should not be
offered as if it could.

The third record is `.smoll/memory.jsonl`: the short sentences `memory_store`
kept for the runs after this one, each one secret-masked before it reached disk
and read back, newest last, onto the next run's first turn. `smoll doctor` reports
how many are waiting. Rollback leaves it alone on purpose — a run that was wrong
about the plan is still worth remembering.
