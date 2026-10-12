# Releases: cutting one, and what is inside it

Nothing in this repository has been released yet. `git tag` is empty, the
Release workflow has never run, and every "download the latest release" sentence
anywhere in these docs is about the tag that will be cut, not one that exists.
That is worth stating before a page about releases, because a release page is
where a project usually starts describing builds it has never made.

This page is the two things that actually matter about that: how to cut the
first one, and which parts of it have been run by a human being on a real
machine rather than assumed.

## What a `v*` tag builds

`.github/workflows/release.yml` has three jobs.

| Job | Runner | Produces | Built with |
| --- | --- | --- | --- |
| `cli` | `ubuntu-latest`, `macos-latest`, `windows-latest` | `smoll-<tag>-linux-x86_64.tar.gz`, `…-macos-aarch64.tar.gz`, `…-windows-x86_64.zip`, each with a `.sha256` beside it | `cargo build --release --locked -p agent-cli --bin smoll --features openai-compatible`, on each platform's own runner |
| `bundles` | `macos-latest`, `windows-latest` | the Tauri `.dmg`/`.app` and `.msi`/`.exe` bundles | `tauri-apps/tauri-action@v1`, unsigned |
| `release` | `ubuntu-latest` | the GitHub Release, with every artifact above attached | runs on a `v*` tag only |

Two details in that table are decisions rather than defaults.

**Three native runners, not one cross-compile.** The default `smoll` build is
pure Rust and could be crossed from any machine, but the feature this ships with
— `openai-compatible` — pulls `rustls` and `ring`, whose C wants the target's own
toolchain. So each archive is built by a machine that is that platform. The
default-feature build for `x86_64-pc-windows-msvc` and `aarch64-unknown-linux-gnu`
was checked with `cargo check -p agent-cli --target …` from a mac; the
feature-carrying build was not, and that is the reason the matrix exists.

**`--locked`.** A release is built from the lockfile that passed the CI gates,
not from whatever resolution today's index happens to give. This has a
consequence in "Cutting a release" below: the lockfile has to be regenerated
when the version is bumped, or the release build refuses to run.

## What a downloaded `smoll` can and cannot do

Read this before promising anyone an answer from a binary on that list.

- It runs the loop: `smoll task`, `smoll init`, `smoll doctor`, `smoll tools`,
  `smoll review`, `smoll rollback`, `smoll eval`, all 24 tools, the sandbox, the
  audit log and the session records in `.smoll/`.
- It answers with `mock` and `echo` — which is a scripted answer, not a model —
  and with `openai-compatible`: a server already running on your machine that
  speaks the OpenAI wire (`llama-server`, LM Studio, Ollama's OpenAI endpoint,
  or this repository's own `smollm serve` in a `--features openai-compatible`
  build).
- It does **not** load weights. `gguf` and `llama-cpp` need a feature build with
  that flag named and a C++ toolchain and cmake, because compiling llama.cpp is
  not something a downloaded archive can promise. See
  [providers.md](providers.md).
- There are no model weights in any archive, and installing one downloads none.

How to find out which of those your binary is, from the binary itself: `smoll
doctor` prints the provider that would answer a run. Ask it for a kind the build
left out and it says so instead of quietly substituting something — the exact
message this build path produces is
`openai-compatible providers need this build to be compiled with --features openai-compatible`,
and the run exits 2 rather than starting. A scripted stand-in answering for your
model would be a lie told with your repository in its context, so the seam
refuses.

## Install paths that work today

There is no binary to download yet, so "install" means building from a checkout.
The first three are what to use today and all three were run here; the fourth is
what checking a real release looks like, rehearsed against an archive this
machine built itself.

```bash
# 1. Run it out of the build tree, no install step.
cargo build -p agent-cli --bin smoll --features openai-compatible
./target/debug/smoll --version

# 2. Onto your PATH. `--features` is not optional if you want a real answer:
#    the default build has no backend, deliberately.
cargo install --path crates/agent-cli --bin smoll --features openai-compatible

# 3. The scripts: both CLIs, release-optimised, into ~/.local/bin (or
#    %LOCALAPPDATA%\Programs\smollm on Windows), from this checkout — no
#    download, no sudo.
./scripts/install.sh
powershell -ExecutionPolicy Bypass -File scripts\install.ps1

# 4. From an archive, once a tag exists:
tar -xzf smoll-v0.1.0-macos-aarch64.tar.gz && cd smoll-v0.1.0-macos-aarch64
shasum -a 256 -c smoll-v0.1.0-macos-aarch64.tar.gz.sha256 && ./smoll doctor
```

`cargo install` without `--features openai-compatible` also works and installs
faster; it just cannot reach a server, so `smoll task` will only ever be
answered by `mock` or `echo`. Rust 1.77 is the minimum for either.

## Cutting a release

1. **Run the gates, not a subset.** The six from
   [CONTRIBUTING.md](../CONTRIBUTING.md)'s "Run the checks" — `cargo fmt`, the
   workspace clippy, `cargo test --all`, the two `agent-providers` feature
   commands, and the desktop `typecheck`/`lint`/`build`. The app ships in the
   same release even though it is frozen. Then `smoll eval`, and the
   `llama-cpp` set if the engine changed since the last tag.
2. **Bump the version in the three places that hold it**, and then let the
   lockfile catch up:

   ```bash
   # Cargo.toml [workspace.package] version, desktop/package.json,
   # desktop/src-tauri/tauri.conf.json — all three, or the app and the CLI
   # disagree about which release they are.
   cargo build -p agent-cli --bin smoll   # rewrites Cargo.lock with the new version
   ```

   Do not skip the second half. `cargo build --locked` against a bump whose
   lockfile still says the old version fails before it compiles anything:

   ```
   error: cannot update the lock file Cargo.lock because --locked was passed to prevent this
   ```

   That message was produced here on purpose, by bumping `0.1.0` to `0.1.1` and
   running the release build command, then putting both files back. It is written
   down because the alternative is discovering it on the runner after pushing the
   tag, which is the only time the tag can be quietly deleted.
3. **CHANGELOG.md**: move everything under `## [Unreleased]` into
   `## [X.Y.Z] – YYYY-MM-DD`, keep the honest tense, and leave a fresh empty
   `Unreleased` above it.
4. **Commit, then tag, then push both.** Pushing a tag is the moment a release
   becomes public and irreversible without deleting it from under anyone who has
   downloaded it, so this step belongs to a person who means it, not to a tool
   that finished a milestone.

   ```bash
   git tag -a v0.1.0 -m "smoll 0.1.0"
   git push origin master v0.1.0
   ```
5. **Watch the run**, and read the artifacts rather than the green tick: the
   Actions tab of the repository, or `gh run watch` where the `gh` CLI exists.
   The `release` job needs
   `contents: write`, which the workflow already asks for; the archives appear
   on the Release only after all three `cli` runners and both `bundles` runners
   succeed, because the job `needs` them.
6. **Check the first download yourself** with step 4 of the install list, on a
   machine that is not the one that built it.

`workflow_dispatch` on the same workflow builds the artifacts without making a
Release, which is the way to test any of this before it is true.

## What was run here, and what only a runner can run

The release notes for the first tag say "the workflow that made these ran for the
first time on this tag". That sentence is load-bearing, so the split below is
what backs it up. Everything in the first list was run on an Apple M1 mac, on
2026-10-12, at `711d1a2` plus this milestone's changes — which touch no `.rs`
file, so the binary checked below is the binary that commit builds — and with the
exact commands from the workflow file. The second list is what no local machine
can stand in for.

**Run here**

- `cargo build --release --locked -p agent-cli --bin smoll --features
  openai-compatible` — exit 0, 1m23s, `reqwest`/`rustls`/`ring` compiled, so the
  feature really is in the binary.
- The `Package` step, verbatim: the archive was 3,724,789 bytes containing
  `smoll`, `LICENSE`, `NOTICE`, `README.md`. This mac has `sha256sum`
  (coreutils), so it took the first branch; the `shasum` branch a bare macOS
  runner takes was exercised separately — both tools printed the same hash, and
  each one's `-c` accepted the other's file, which is what the release notes
  claim when they tell you either works.
- The extracted binary, from inside the extracted directory, not the build tree:
  `--version` → `smoll 0.1.0`, exit 0; `doctor` → exit 0, reporting mock,
  approve-edits, strict sandbox, 24 of 24 tools, and the machine it was on;
  `tools` → exit 0, the same 24 at 2641 prompt tokens; `task "…"` → exit 0, a
  completed run and a JSON record under `.smoll/sessions/`; `review` → exit 0,
  reading that record back.
- `chat` with its stdin pointed at a pipe: exit 2 and
  `there is no terminal to draw on here; smoll task … runs the identical loop as
  printed output` — the refusal a CI job or a `|` needs to give rather than
  hanging with the terminal in a strange state.
- `cargo install --path crates/agent-cli --bin smoll --features
  openai-compatible --root <throwaway directory>` → exit 0, and the installed
  `smoll` in that directory answers `--version` and `tools`. Nothing was
  installed into `~/.cargo/bin` by that check.
- The `--locked` behaviour of a version bump, as described in step 2 above.
- `./scripts/install.sh --prefix <throwaway directory>` → exit 0, 47s of release
  build, `~/.local/bin`-shaped `bin/` containing both `smollm` and `smoll`, each
  printing its version at the end. The `--help` text and `bash -n` too. Nothing
  was installed into the real `~/.local/bin` by that check.
- The release Notes step's script, extracted from the workflow YAML and run
  under `bash -e` with `GITHUB_REF_NAME` set: exit 0, correct markdown. That run
  is also how a backtick in one of those `echo` lines got caught before it
  shipped — unescaped, a backtick in a double-quoted `echo` is a command
  substitution, and `echo "… \`sha256sum\` …"` would have had the step reading
  from a stdin that never arrives.

**Only a runner can run**

- The Linux and Windows `smoll` builds, and the Windows `Package` step
  (`Compress-Archive` + `Get-FileHash`).
- `scripts/install.ps1`, which gained the same `smoll` build and install as
  `install.sh`. This mac has no PowerShell in `PATH`, so that file was read, not
  run — the first Windows contribution or runner pass is what proves it, and
  saying so here is cheaper than a doc that implies both scripts were tested.
- Every Tauri bundle, and therefore the `release` job's `files:` list — its
  `fail_on_unmatched_files: true` is precisely there to notice a bundle path that
  does not match, which is a thing only the run that produces them can tell you.
- Uploading artifacts, creating the GitHub Release, and the download-and-check
  round-trip a real user does.
- Anything signed: the `bundles` job is deliberately given no signing secrets, so
  every bundle from this workflow is an unsigned local build until
  `APPLE_*`/`WINDOWS_*` secrets exist. A first-time download needs right-click →
  Open on macOS.

So: the `smoll` archive's contents are verified. The act of publishing is not.
Cut the first tag with `workflow_dispatch` (artifacts, no Release), download one
onto a second machine, and only then push the tag.

## Not done here, on purpose

- **No Intel macOS bundle.** The matrix has one mac runner and it is arm64, so no
  `x64.dmg` is built and none should be named. Both places that used to name one —
  the README quickstart and this page's own install table — now say what a tag
  really produces instead. Leaving the sentence would have been the worse bug: a
  reader who downloads a file that was never built has nothing to check a hash
  against.
- **No `llama-cpp` archive.** It would need cmake and a C++ toolchain on the
  build machine and a promise about the one on yours; that is roadmap item 5,
  which is signed-notarised-bundles-with-the-real-engine, and it is still the
  roadmap rather than this workflow.
- **No auto-update.** The Tauri updater plugin is not wired; the app notices a
  newer release and tells you, which is the honest version of the same need.
