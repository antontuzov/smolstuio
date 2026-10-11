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
| `gguf` | `gguf`, `llama-cpp` | **built** — loads the configured file through this repository's own engine layer and answers from its weights. Without `llama-cpp` in the build it refuses and names that flag; it never answers from the mock. |
| `openai_compatible` | `openai-compatible` | **built** — one `POST /v1/chat/completions` per step, to Ollama, LM Studio, vLLM, TGI, `llama-server` or this repo's own `smoll serve`. Without the feature in the build it refuses and names that flag; it never answers from the mock. |
| `candle`, `onnx`, `huggingface` | none | **not built**, and no feature claims otherwise |

An unbuilt kind is an error with a name in it, never a fallback. This is the one
design decision worth arguing about: a provider that quietly substituted the
mock for your model would keep the demo running and produce confident nonsense
about your repository. `agent_providers::build` would rather stop and say which
adapter is missing:

```rust
// From a default build, which has no engine adapter compiled in at all.
let config = ProviderConfig::new(ProviderKind::Gguf);
let error = build("local", &config).unwrap_err();
// gguf providers need this build to be compiled with --features gguf
assert!(!error.retryable());
```

That message is what a default build prints, with the provider's own name in
front of it:

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

## GGUF: the provider that reads your own file

`GgufProvider` is an adapter, not a second inference engine. Everything hard —
loading the weights, the model's chat template, its tokenizer, the decode loop,
offloading layers to the GPU — is done by `smollm-engine`'s llama.cpp backend,
the same one the desktop app and `smollm run` use. The adapter's job is the
translation between the loop's request and the engine's, and the bookkeeping
around it.

```toml
[providers.local]
type = "gguf"
path = "~/models/qwen2.5-1.5b-instruct-q4_k_m.gguf"
# context_length, temperature, top_p, max_tokens, stop and device are all
# optional; each one the file can answer for itself is left to the file.
device = "metal"
```

```console
$ cargo build -p agent-cli --bin smoll --features llama-cpp --release
$ smoll task "rename the helper in src/config.rs and update its callers"
```

What it will not do is answer from something else:

- A build without `llama-cpp` in it says so and names the flag. `--features
  gguf` alone compiles the adapter against the engine layer, which is how its own
  tests run: they inject the mock engine by hand, and a `type = "gguf"` section
  read from a config file never gets one.
- A `path` that is not a readable file is refused when the provider is built,
  before any weights are touched, and the error keeps the path in it.

The details that decide whether a run is worth reading:

- **One load per run.** The model is read on the first step, off the async
  executor so a multi-gigabyte mmap cannot stall the TUI's input pump, and every
  later step reuses that handle. `smoll doctor` builds providers too, which is
  why loading is lazy rather than part of `build()`.
- **The file's own window.** `context_length` from the config is a ceiling until
  the model is loaded; after that the loaded handle's context — which llama.cpp
  already capped at what the weights were trained with — is what the loop trims
  against.
- **Numbers the model actually paid.** `usage` comes from llama.cpp's tokenizer,
  not a character estimate. The pre-check that a prompt fits uses the same
  tokenizer and the engine's own `+ 1` arithmetic (one position has to stay free
  for the first sampled token), so an over-long prompt is `ContextTooLong {
  wanted, available }` with real numbers instead of an error in prose.
- **A stopped run stops the decoder.** Dropping the stream — the loop's timeout,
  or a step budget that ends the run — makes llama.cpp's decode worker finish
  between tokens, because it notices its consumer went away. A caller that owns
  the engine's cancel token (`GgufProvider::with_cancel`) gets `Cancelled` out of
  the answer loop instead, which is not retryable: a run a human stopped is not a
  run that failed. The `Provider` trait has no way to pass that token yet, so
  today only a program that builds the adapter itself can pull that wire — the
  TUI's Esc key is the caller it is there for.
- **`tool_calling` is false, on purpose.** No GGUF file has said it can hold a
  tool list, and the adapter does not read calls out of a template. The loop
  therefore asks for prose with a fenced JSON block and parses that back, which
  is the path most 1.5B chat models answer through anyway. `streaming` is false
  for the same reason: the trait has one answer per step, so a token-by-token
  engine still reports one finished answer.
- **No seed unless you ask.** The default coding preset's fixed seed is dropped,
  because a repair round with a fixed seed re-asks the same question and gets the
  same truncated answer back.

## OpenAI-compatible: the provider that asks a server

`OpenAiProvider` is the one adapter here that talks to something outside this
process, which is why it is the one with a rule about secrets.

```toml
[providers.ollama]
type = "openai_compatible"
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:1.5b"
# optional: temperature, top_p, max_tokens, stop, context_length
# A server that wants a key is given the NAME of a variable, never a value:
# api_key_env = "OLLAMA_API_KEY"
# And a server is only handed a tool list once you say it can fill one:
# tool_calling = true
```

```console
$ cargo build -p agent-cli --bin smoll --features openai-compatible --release
$ smoll task "rename the helper in src/config.rs and update its callers"
```

Any of these answers that request, because all of them speak the same wire:

```console
$ ollama serve                                # http://localhost:11434/v1
$ lm-studio-server                            # http://localhost:1234/v1
$ vllm serve Qwen/Qwen2.5-Coder-1.5B-Instruct # http://localhost:8000/v1
$ llama-server -m ~/models/small.gguf         # http://localhost:8080/v1
$ smollm serve                                # this repository's own server
```

Those two binaries were run against each other, which is the check that says the
wire is read correctly rather than merely written:

```console
$ smollm serve --engine mock --port 18787
$ smoll -C /tmp/demo task --yes "describe what this repository holds"
  0 tool call(s), 0 file(s) touched
record    /tmp/demo/.smoll/sessions/84444ac3-….json
```

One `POST /v1/chat/completions` left this process and one answer came back; the
server counted the request and the tokens it generated, and the run wrote a
session record like any other provider. `--engine mock` answers are labelled
`simulated` by the engine itself, so nothing in that transcript reads as a model
that was never run. A URL with nothing behind it costs
`cannot reach http://…/v1/chat/completions: nothing is listening there` and a
retry or two, and a section with no `base_url` never reaches the socket at all —
`agent-config` refuses it, and `smoll` exits with the code a CI job reads as "not
run like this".

- **`base_url` is written three ways and all three work.** `http://host:11434`,
  `http://host:11434/v1` and the full endpoint are each finished off to
  `/v1/chat/completions`, because each spelling is what a different server's own
  README tells you to paste. A prefix under the version (`http://host:8000/api/v1`)
  is kept, and a value that is not an `http` or `https` URL is refused before
  anything is asked.
- **A key goes to one host, and to that host only.** The bearer is read from the
  variable the config names, at build time — so a run costs one line of output
  rather than its first step when the variable is not set — and redirects are not
  followed at all. A server that answers `302` gets the refusal reported instead
  of a second request, because following a redirect would hand the header to a
  host your config never named. `Debug` for the provider prints the name, the
  model and the URL, and the key as a bracketed note: a run's log, an error's
  debug output and a panic hook all print providers.
- **Nothing is assumed about a tool list.** `tool_calling` defaults to false, so
  `smoll serve` and TGI — which answer a request without reading a `tools` field
  — get the prose-and-fenced-JSON path the loop already uses. Say `true` for a
  server you know fills one, and then the calls that come back are still filtered
  by the list that was offered: a reply naming `delete_everything` when only
  `list_files` was on offer produces no call at all, and arguments that are not a
  JSON object are dropped rather than run as `{}`. Both the nested
  `{"function": …}` shape and the flat `{"name", "arguments"}` shape are read, with
  the arguments either as a JSON string or as an object, because servers do not
  agree.
- **The server's own numbers when it gives them.** A `usage` block is taken as
  what the model paid; a server that sends none, or one of zeroes, is charged the
  same character estimate the loop's budget check uses, so a run's token total is
  never free on paper.
- **`finish_reason` is read, not guessed.** `length` is a truncated answer, which
  the loop can answer by raising the budget; `tool_calls` is a call. A matched stop
  sequence and a finished turn are both `stop` on this wire, so `StopSequence` is
  not something a server can tell us — the same conclusion the GGUF adapter
  reaches from the engine.
- **Status codes decide whether asking again makes sense.** `401` and `403` are a
  refusal, `400` about a context length is `ContextTooLong`, and everything else
  that is 4xx is an unusable reply — none of them retried. `429` carries the
  server's `Retry-After` into `backoff_hint()`, and a connection failure or a 5xx
  is `Unreachable` with the full URL in it, because "is it running, and on which
  port" is the question that answer decides.
- **No timeout of its own.** The loop owns the run's budget; a second deadline in
  the client would only disagree with it.

## A file this engine cannot load, and the route that works

`prism-ml/Ternary-Bonsai-2-27B-gguf` is a 27B model that fits in 5.95 GB, and it
is worth walking through because the conclusion generalises: a GGUF file whose
quantisation this machine's llama.cpp does not know is not a file to force
through. It is a file that needs the server its own authors ship.

What was checked, and how: the Hub API, the model card, its `KNOWN_ISSUES.md`,
and the first 8 MB of `Ternary-Bonsai-2-27B-PTQ1_0.gguf` read with one HTTP
`Range` request. Nothing was downloaded and no weights were run here, so every
number below is either that file's own header or the model's own documentation.
There is no throughput claim in this section and none should be inferred from it.

- Not gated, Apache-2.0, base model `Qwen/Qwen3.8-27B`. No token is needed to
  fetch it, so it never goes near the keychain.
- The header: GGUF v3, 851 tensors, `general.architecture = qwen35`, 64 blocks,
  `context_length = 262144`, 5120 embedding, 24 attention heads with 4 KV heads,
  sampling baked in at temp 1.0 / top_p 0.95 / top_k 20.
- The 5.95 GB pack is **`PTQ1_0`** — PrismML's own dense-trit packing at 1.75
  bits/weight. It is *not* llama.cpp's stock ternary type `TQ1_0`; the names are
  one letter apart and the similarity is a trap. The repo also ships `PQ2_0`
  (2.13 bits, 7.21 GB), the `F16` original (50.1 GiB) and two `mmproj` vision
  projectors.
- The card, verbatim: *"Stock llama.cpp will not run these files. It rejects
  `PQ2_0` and `PTQ1_0` as unknown types, and it loads `Q2_0` without any warning
  and produces garbage, because it has no Hadamard activation runtime."* The
  weights are stored in a rotated basis — a blockwise Hadamard rotation folded in
  at quantisation time — so a runtime either applies the matching transform to
  activations or has no business loading the file.

Our `type = "gguf"` provider links stock llama.cpp, so it *is* the binary that
card refuses. This repository's own header reader names every quantisation up to
`Q2_0` and reports anything past the end as unknown rather than guessing at it,
which is the right outcome: a wrong name is how a bad file becomes nonsense
output. The route that works is the one two sections above already describe —
their fork's server, our `openai_compatible` provider:

```console
$ git clone https://github.com/PrismML-Eng/llama.cpp && cd llama.cpp
$ cmake -B build -DGGML_CUDA=ON && cmake --build build -j   # drop the flag on macOS, Metal is default
$ ./build/bin/llama-server -m Ternary-Bonsai-2-27B-PTQ1_0.gguf -ngl 99 -c 65536 --port 8080
```

```toml
[agent]
max_context_tokens = 65536

[providers.bonsai]
type = "openai_compatible"
base_url = "http://localhost:8080/v1"
model = "ternary-bonsai-2-27b-ptq1_0"
context_length = 65536
tool_calling = true
max_tokens = 16384
```

Nothing in that section is special to this model and nothing in this repository
was changed to make it work. It has not been run from this page either: no weights
were downloaded here, so the commands are transcribed from what the model's own
card tells you to do, and this document still makes no claim about an answer from
real weights on this machine. The keys that decide whether the run is usable are
`tool_calling` and `max_tokens`, and the second is the difference between an
answer and an empty one.

**A thinking model needs a large output budget, and it takes two numbers to give
it one.** Its own issue list opens with *"Empty or truncated answers… the
reasoning counts against the output limit… This is the most common report we
see"*, and its workaround is `-n 16384` with `-c 65536`. The loop's default reply
budget is `DEFAULT_REPLY_TOKENS = 256`, which is exactly the cap that ends a
thinking model mid-thought. `max_tokens` in the provider section overrides it, but
`Limits::from_config` clamps any answer budget above a third of
`[agent] max_context_tokens` down to a sixth of it — an answer that eats a third
of the window leaves nothing to look at — so `max_tokens = 16384` against the
default 16000 silently becomes 2666. Raise `max_context_tokens` with it, or the
override does not arrive.

Three things elsewhere in this repository misjudge a file like this one. They are
documented here because knowing what the output means is cheaper than trusting it:

- `smollm models verify` has a plausibility floor of 0.3 bytes per parameter and a
  ceiling of 8. A 1.75-bit pack measures about 0.22, so the check **fails a
  legitimate file** with "outside the 0.3–8.0 range any quantisation reaches".
  The file is fine; the range predates ternary packs this dense.
- The RAM estimate in [hardware.md](hardware.md) charges for full attention on
  every layer. For a backbone its own card describes as mostly linear attention,
  that gives a KV cache of roughly 53 GiB at the file's declared 262144-token
  context, against a real need far lower — most of those layers keep a
  fixed-size recurrent state, not a growing cache. Read the number as an upper
  bound for a dense-attention model.
- A model hint containing `qwen` selects the Qwen2 chat template, so an
  architecture named `qwen35` is read as a Qwen2 file. Through a server this
  means nothing, because the server owns its template; it matters the moment a
  template-reading path meets an architecture name this repository has not seen.

Two more notes from its issue list land directly on this agent loop and are
already satisfied here, which is worth recording so they stay that way: the chat
template answers HTTP 500 unless there is **exactly one system message and it is
first** (the loop builds one and only one), and a tool call whose `arguments` are
empty or not JSON is a 500 (a call with no arguments serialises `{}`). Two are not
ours to fix: malformed tool calls, and reasoning re-rendered into later turns
defeating the prompt cache in a tool loop, are logged there as open model-side
issues — and the advice not to echo reasoning back is one this provider already
follows, since it reads `content` and `tool_calls` and never carries a
`reasoning_content` field into the history it sends.

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
| `Unconfigured { message }` | no | the config section itself is wrong: no path, no URL, a key variable that is not set |
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
