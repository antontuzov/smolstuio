<!--
Everything below the line is the template. Delete nothing above it — the gate
list is the reason a review here takes two minutes instead of two days.
-->

## What changed

One paragraph. Say what a person gets, not which files you opened — "a run can
now be stopped from the screen, and the record says so" is reviewable, "touched
`app.rs`" is not.

## The gates

CI runs exactly these. A green `cargo test --all` alone is not enough, and pasting
a wall of output is not either — tick them when they pass on your machine.

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --all`
- [ ] `cargo clippy -p agent-providers --all-targets --features gguf,openai-compatible -- -D warnings`
- [ ] `cargo test -p agent-providers --features gguf,openai-compatible`
- [ ] `cd desktop && pnpm typecheck && pnpm lint && pnpm build` (only if you
      touched the frozen app — and if you did, say why in the description)

The `llama-cpp` gates are a separate CI job, because compiling llama.cpp needs
cmake and a C++ toolchain rather than being something every machine has. If your
change touches the engine or the gguf adapter, `CONTRIBUTING.md` lists the
commands that job runs, and saying which of them you ran belongs in the
description above.

Two more, when you touched the loop, the policy or the prompt:

- [ ] `smoll eval` — the safety promises re-run against the real loop. It is
      asserted by the CLI suite, so a broken promise fails `cargo test --all`
      anyway; running it by hand shows you which one.
- [ ] `cargo bench -p agent-repo` (and `-p agent-tools`, `-p agent-sandbox`) if
      you changed what a turn costs. These print timings and assert nothing on
      purpose — do not paste them into a doc as a baseline.

## Promises this moves

The docs quote measured numbers, so a change that moves one has to sweep the
others in the same commit, or the repository starts disagreeing with itself:

- the tool count and its per-step price (`docs/tools.md`, `README.md`,
  `CONTRIBUTING.md`, `crates/agent-tools/src/registry.rs`'s upper bound);
- the exit-code contract (0 ran, 1 finished and did not work, 2 refused to start)
  and the command list in `README.md`, `docs/agent-config.md`, `docs/getting-started.md`;
- what an engine or provider can actually do — if you changed what "available"
  means, `docs/providers.md` and `docs/models.md` say it somewhere.

- [ ] I searched the docs for the number, count or claim I changed, and swept it.
- [ ] New figures came from a command I ran, and I named the command next to them.

## What this does not do

Say the quiet part yourself; a reviewer finds it either way.

- [ ] Nothing added sends repo content, a prompt or a telemetry ping anywhere that
      config did not name. No telemetry, no analytics, no default cloud call.
- [ ] No key, token or credential in a file, a test fixture, a log line or a
      commit. Config may name an **environment variable**, never a value.
- [ ] Nothing that writes, deletes or runs got in without saying which approval
      mode asks about it and which sandbox rule can refuse it.
- [ ] No capability claim resting on weights nobody loaded. If a number is not
      measured yet, it is "coming soon" — see `docs/eval.md`.

## Constraints that bind a dependency

- **rust 1.77 is the floor.** A new or bumped dependency has to build there, which
  is why `ratatui` sits at 0.29 and `crossterm` at the line 0.29 itself requires.
- **The desktop app is frozen**: it keeps passing gates and gets no features while
  the agent is built.
- `desktop` installs through **corepack pnpm** only; `package-lock.json` or a yarn
  lockfile in a PR here is a mistake, not a second supported path.

## Verification

How did you check it beyond the gates? The rule on this machine is to run the real
binary — `cargo run -p agent-cli --bin smoll -- …` — rather than reason about what
it would have done. If something only a terminal or a real model can show was not
checked, say so here and it stays out of the docs.

Fixes #____
