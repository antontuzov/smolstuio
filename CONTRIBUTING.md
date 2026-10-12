# Contributing

Thanks for being here. SmolLLM Studio is a small, local-first tool and it should
stay that way. This document covers how to get a working dev loop and the rules
this repo is held to.

## Get set up

You need:

- Rust 1.77 or newer (`rustup show` should print the workspace's toolchain)
- Node.js 20+ and pnpm (`corepack enable` provides pnpm without a global
  install; there is no committed `packageManager` pin, so any recent pnpm works)
- Python 3 with Pillow, only if you want to regenerate the app icons
- The Tauri v2 system prerequisites for your OS:
  <https://tauri.app/start/prerequisites/>

```bash
git clone https://github.com/antontuzov/smollm-studio
cd smollm-studio
cargo build                          # workspace libraries + the smollm CLI
cd desktop && pnpm install           # frontend deps
pnpm tauri dev                       # native window + Vite on :1420
```

`pnpm dev` alone runs the UI in a browser with every command failing into its
error state. Useful for styling, useless for data — the Tauri IPC bridge does
not exist there.

Both CLIs are first-class, so most work can be verified without a window at all:

```bash
cargo run -p smollm-cli --bin smollm -- hardware
cargo run -p smollm-cli --bin smollm -- models list
cargo run -p agent-cli --bin smoll -- doctor
cargo run -p agent-cli --bin smoll -- tools
```

## Run the checks

CI enforces exactly these. Run them before you open a pull request — a green
`cargo test` is not enough on its own.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --all
cargo clippy -p agent-providers --all-targets --features gguf,openai-compatible -- -D warnings
cargo test -p agent-providers --features gguf,openai-compatible
cd desktop && pnpm typecheck && pnpm lint && pnpm build
```

The two `agent-providers` lines are there because `--workspace` above builds that
crate with its default features, and its GGUF and HTTP adapters are behind
flags: without them, an adapter that only ever compiles in a feature nobody runs
is a change nobody checked. They build against the engine layer's mock, so they
need no C++ toolchain.

The `llama-cpp` set needs cmake and a C++ toolchain, so it is a separate CI job
on Linux and macOS rather than one of the six above. Run it yourself if you
touched the engine, the desktop shell's linking, or the gguf adapter — these are
the commands that job runs, one package at a time:

```bash
cargo clippy -p smollm-engine --all-targets --features llama-cpp -- -D warnings
cargo clippy -p smollm-cli --all-targets --features llama-cpp -- -D warnings
cargo clippy -p agent-providers --all-targets --features llama-cpp -- -D warnings
cargo clippy -p agent-cli --all-targets --features llama-cpp -- -D warnings
cargo clippy -p smollm-studio-desktop --all-targets --features llama-cpp -- -D warnings
cargo test -p smollm-engine --features llama-cpp
cargo test -p agent-providers --features llama-cpp
```

The desktop-shell line is the one CI only runs on macOS: a plain Ubuntu runner
has no Tauri system libraries. Everything else above runs on Linux and macOS.

Two commands are worth running when you touched the loop, the policy or the
prompt. The first is already covered by the gates above; the second is not, and
is only yours to read:

```console
$ smoll eval                  # the safety promises, re-run against the real loop
$ cargo bench -p agent-repo   # and -p agent-tools, -p agent-sandbox for timings
```

`smoll eval` is asserted by the CLI test suite, so a promise it reports broken
fails `cargo test --all`. The benches are deliberately not: they print what they
measured and assert nothing, because a stored time baseline goes stale on the
next machine and there is no number here to keep in step. See
[docs/eval.md](docs/eval.md).

The screen is tested without a terminal. `crates/agent-tui/tests/screen.rs` draws
a frame into `ratatui`'s `TestBackend` and reads that buffer back cell by cell;
the crate's own tests cover the model, the layout maths, and the keys through
`press()`, which takes a screen and a key event and returns an effect, so no run is
involved. A change to any of them has to keep the two rules these tests exist for:
a question nobody answered is a no, and the terminal comes back on every path out.
Driving the real binary needs a pty, which CI does not have, so that check stays a
local one and is described in [docs/tui.md](docs/tui.md).

Clippy is configured to fail on warnings, and `lint` runs with
`--max-warnings=0`. If a lint genuinely needs suppressing, use a scoped
`#[allow]`/`eslint-disable` with a one-line reason, never a blanket ignore.

## The rules that matter

These are not style preferences; they are the product's guarantees.

1. **No `unwrap()` in production code.** Tests and genuinely impossible
   invariants are the only exceptions, and they need a comment saying which.
2. **Avoid unnecessary `unsafe`.** If you think you need it, explain why in the
   PR before writing it.
3. **Never block the Tauri main thread.** Commands that do I/O or long-running
   work must be `async` and must not hold a lock across an `.await`.
4. **Use `tracing` for logs**, not `println!`/`dbg!`.
5. **No telemetry. Nothing leaves the machine except a model download from the
   registry you chose.** No analytics, no update pings, no crash reporting. A
   PR that adds an outbound request which isn't a user-initiated model fetch
   will be closed.
6. **Local by default.** Anything new must work with the network off; a network
   dependency is an explicit, opt-in feature.
7. **Do not rewrite inference.** Integration goes through the `Engine` trait in
   `crates/smollm-engine`. Adapters belong behind their feature flags
   (`llama-cpp`, `candle`) so the default build stays pure Rust and portable.
8. **If an API or dependency is uncertain, leave a clearly marked TODO adapter
   rather than breaking the build.** An honest seam beats a fake implementation.
9. **Be honest in user-visible copy.** Don't claim a capability the build doesn't
   have; see the README's "what is real, and what is a seam".

## Commits and pull requests

- **Semantic commit messages**: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `perf:`, `build:`, `ci:`, `chore:`. Scope is optional but welcome —
  `fix(server): ...`.
- One logical change per commit. Keep `cargo fmt` output in the same commit as
  the code it formats.
- Add a `CHANGELOG.md` entry under "Unreleased" for anything user-facing.
- Describe what you verified, and how. For UI changes, say which theme you
  looked at — light is the default and it is easy to only ever test dark.

## Releases

Cutting one is [docs/release.md](docs/release.md): which three files hold the
version number, why the lockfile has to be regenerated before the tag or the
release build refuses to run, what each archive contains and which feature it
left out, and which parts of the pipeline have been run by a person on a real
machine rather than only written down. Pushing a tag is the step that makes a
release public, so it is a human decision and never something a tool does at the
end of a milestone.

## Where to look first

- `crates/smollm-core` — the shared vocabulary: `AppError` and its codes,
  `Settings`, `AppPaths`.
- `crates/smollm-engine/src/lib.rs` — the `Engine` trait and `EngineManager`.
  Start here for any inference work.
- `crates/smollm-models` — catalog schema and the download manager.
- `desktop/src-tauri/src/commands.rs` — the command surface;
  `docs/api.md` lists all 44 with their events.
- `desktop/src/lib/api.ts` — the single place the frontend crosses into Rust.
  Keep the TS types mirroring the Rust structs.

Adding a Tauri command means touching all three: Rust command, `generate_handler!`
registration, and `api.ts`. Don't call `invoke` from a component directly.

## Get in touch

Open an issue before starting anything that changes the surface area (a new
page, a new command, a new backend flag, a dependency swap). SmolLLM Studio is
deliberately lightweight — "we could" is not the same as "we should".
