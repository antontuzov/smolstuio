# Providers

One trait stands between the agent loop and whatever is producing text:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn model(&self) -> Option<&str> { None }
    async fn complete(&self, request: &CompletionRequest) -> ProviderResult<Completion>;
    async fn complete_again(&self, request: &CompletionRequest) -> ProviderResult<Completion>;
}
```

The loop asks for one completion per step and gets back text plus whatever tool
calls it recognised. That is the whole contract: nothing here knows about
planning, retries across steps, approval prompts or the repository.

The crate is `agent-providers`. Everything in it is local-first — no provider
sends anything anywhere that the configuration did not name.

## Status

| Kind (`type = "…"`) | Build feature | State |
| --- | --- | --- |
| `mock` | none, always compiled | **built** — scripted replies, records every request |
| `echo` | none, always compiled | **built** — renders the prompt back so you can read the exact context |
| `gguf` | `gguf`, `llama-cpp` | **not built** — `build()` returns `ProviderError::not_supported`; the engine adapter is the next step |
| `openai_compatible` | `openai-compatible` | **not built** — the HTTP client for Ollama, LM Studio, vLLM, TGI, `llama-server` and `smoll serve` is next |
| `candle`, `onnx`, `huggingface` | none | **not built**, and no feature claims otherwise |

An unbuilt kind is an error with a name in it, never a fallback. This is the one
design decision worth arguing about: a provider that quietly substituted the
mock for your model would keep the demo running and produce confident nonsense
about your repository. `agent_providers::build` would rather stop and say which
adapter is missing:

```rust
let config = ProviderConfig::new(ProviderKind::Gguf);
let error = build("local", &config).unwrap_err();
// gguf providers need this build to be compiled with --features gguf
assert!(!error.retryable());
```

That message is what a run prints today, with the provider's own name in front
of it:

```console
$ smoll task --yes "rename the helper"
error: provider local: gguf providers need this build to be compiled with --features gguf
$ echo $?
2
```

`smoll chat` is not a subcommand at all yet. Nothing in this document that is not
in the "built" row above has reached a terminal.

## Capabilities are asked, not assumed

```rust
pub struct Capabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub context_tokens: usize,
}
```

`context_tokens` is the window the model was configured with, not the one it can
be stretched to; overstating it is how a run dies at step nine. A model without
`tool_calling` is not an error. It is a run that gets told to answer in prose
with a fenced JSON block and whose answer then has to be parsed back — which is
exactly what most 1.5B GGUF files need.

The floor is `Capabilities::basic()`: no streaming, no tool calling, 4096
tokens. A provider that can do more says so.

Every request is checked against it before the call:

```rust
pub fn fits(&self, capabilities: &Capabilities, reply_budget: usize) -> bool {
    self.prompt_tokens() as usize + reply_budget <= capabilities.context_tokens
}
```

`prompt_tokens()` uses the same approximation everywhere in this repository, so
the number in the loop, the number in the UI and the number in the audit log
agree. `Request too large` is reported as `ContextTooLong { wanted, available }`
and is deliberately **not** retryable: the caller has to cut context, and asking
the same question again cannot help.

## Tool calls come back in three shapes

`parse::tool_calls(text, is_known)` reads a call out of whatever the model
actually wrote, in this order of confidence:

1. **Native** — the provider itself recognised a function call, so it arrives in
   `Completion::calls` and nothing is parsed. Most small models never produce
   this.
2. **Fenced JSON** — a ```json block, or any brace-delimited object in the text:
   `{"tool": "read_file", "arguments": {"path": "src/main.rs"}}`
3. **Tagged** — `<tool_call>…</tool_call>`, the form some chat templates emit.

Argument keys are matched loosely (`arguments`, `args`, `parameters`, `params`,
`input`, `payload`, `values`), names too (`name`, `tool`, `function`, …), and the
nested `{"function": {"name": …}}` shape OpenAI servers use. A bare
`{"tool": "list_files", "dir": "src"}` is read as a call with the remaining
fields as its arguments.

Malformed JSON gets one repair pass (`repair`): trailing commas removed, Python
and SQL literals (`True`, `None`, `NULL`) substituted, an unterminated string or
unbalanced brace closed — if and only if the fix is unambiguous and the nesting
is no deeper than eight.

### The rule that governs all of it

**A call is only recognised for a tool that was offered.**

```rust
pub fn tool_calls_among(text: &str, tools: &[ToolSchema]) -> Vec<ToolCall>
```

The parser reads what is there; the tool list is what gates it. A model that
hallucinates `delete_repo` into its prose yields nothing at all, because that
name was not in the request. The integration test in
`crates/agent-providers/tests/providers.rs` asserts exactly that, in both
directions: the same text produces zero calls against an honest tool list and
one call when the name is knowingly allowed.

The second refusal: **no invented arguments**. If a call names a known tool but
its argument blob cannot be read, the call is dropped rather than run with `{}`.
A tool that executes with fabricated defaults is worse than a step that has to
be asked again.

## Mock: the provider every test runs against

```rust
let mock = MockProvider::scripted([
    Reply::text("I will read the config first."),
    Reply::call("read_file", json!({"path": "src/config.rs"})),
    Reply::text("The loader is in load_settings()."),
]);
```

Replies are taken one per call, in order. The point is not that it is fake; it is
that it is **accountable**:

- It records every request it receives, so a test can assert that step three was
  asked with more messages than step one.
- It records the request *before* it answers, so a step that fails still leaves
  its prompt on record.
- It parses a scripted `Reply::Text` through the same `tool_calls_among` a real
  provider's prose would go through. The mock cannot flatter the loop.
- It counts the prompt it was given into `usage`, so token accounting is tested
  rather than asserted.
- `remaining()` and `answers_given()` make an over-long run a test failure
  instead of a hang.
- Run out of script and it errors — `Malformed`, not retryable, with the number
  of replies it had already given — unless you gave it a `fixed()` or
  `repeat_last()` policy. It will not make up a fourth answer to keep a
  demonstration alive.

`MockProvider::unscripted()` is what `smoll init` writes into `smoll.toml`, so a
fresh clone can run the loop with no model on disk.

## Echo: the provider that shows your prompt

`EchoProvider` answers by rendering the request back: a `Tools available` header
naming each tool, its description and its argument types, then each message with
its role. No calls, ever — repeating a prompt is not an intent to run anything.

It is the debugging tool for the honest failure mode of a small-model agent,
which is not "the model was wrong" but "the prompt I built was wrong". Point the
configuration at it and read what the context builder actually assembled:

```toml
[providers.inspector]
type = "echo"
context_length = 4096
```

```rust
let provider = build("inspector", &config)?;
print!("{}", provider.complete(&request).await?.text);
```

## Errors, and whether asking again is sensible

`ProviderError` splits by that one question, because a rate limit and a model
that does not exist look identical in a log and want opposite responses:

| Variant | Retryable | Meaning |
| --- | --- | --- |
| `Unreachable { target, message }` | yes | cannot connect, or cannot start the server |
| `RateLimited { message, retry_after_seconds }` | yes | the server asked us to slow down, and how long |
| `Other { message }` | yes | unexpected, worth one more attempt |
| `Unauthorized { status }` | no | no key, wrong key, no permission |
| `Malformed { message }` | no | the reply is not readable |
| `ContextTooLong { wanted, available }` | no | the caller must cut context |
| `NotCompiled { kind, feature }` | no | this binary was built without that backend |
| `NotSupported { kind, why }` | no | this repository does not have that provider yet |
| `Cancelled` | no | a human stopped it |

`retryable()` is false for every variant that describes the **request** rather
than the connection: retrying a bad request produces the same bad answer four
times. `backoff_hint()` returns the server's `Retry-After` where it said one.

`complete_again` exists because a small model's first answer is often unusable
prose. The default re-asks the same question, which is enough for a flaky
server; a provider that can rephrase the request into a stricter format overrides
it. One retry, one more step, and the loop can log both.

## Adding a provider

1. Add a variant to `ProviderKind` in `agent-config` and a name to `as_str()`.
2. Implement `Provider` in `crates/agent-providers/src/<kind>.rs`, behind a
   cargo feature if it drags in a dependency. Keep the default build free of
   heavy inference libraries.
3. Wire it in `build.rs`. Until it works, return `not_supported` naming the kind
   — do not return a mock.
4. `capabilities()` must report the truth about tool calling and the window.
5. Test against the loop's shapes in `tests/providers.rs`: a native call, a
   fenced call, a truncated answer, an over-long prompt and one retryable error.

Nothing in `agent-core` needs to change to add one, which is the point of the
trait.
