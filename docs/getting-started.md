# Getting started

Three things to know before you start:

1. Everything lives on your machine — no account, no key, no telemetry.
2. The default build answers with a **simulated** engine. The UI, downloads,
   server and benchmarks are real; the words are not. Real weights need a build
   with `--features llama-cpp`, which compiles llama.cpp and so needs cmake and a
   C/C++ toolchain. See
   [README — what is real](../README.md#please-read-this-first-what-is-real-and-what-is-a-seam).
3. The app is a 1024×640 minimum window. Below that, the layout starts
   stacking.

## Install

### From a release

No release has been published yet: this repository has no tags, and nothing to
download. What a `v*` tag will build is in
[release.md](release.md) — a `.dmg` and a `.app` from the mac runner (Apple
Silicon: the workflow has no Intel runner, so no `x64.dmg` exists until one is
added), an `.msi` and a `setup.exe` from the Windows runner, and a `smoll`
archive per platform, each unsigned. Until then, build from source below; the
two routes give the same binaries.

Drag the app into `/Applications` on macOS once you have it. An unsigned build
triggers Gatekeeper the first time: right-click → **Open**, or remove the
quarantine attribute with
`xattr -dr com.apple.quarantine /Applications/SmolLLM\ Studio.app` once you are
comfortable doing so.

On Windows the installer needs WebView2, which ships with Windows 11 and most
Windows 10 machines.

### From source

```bash
git clone https://github.com/antontuzov/smollm-studio
cd smollm-studio/desktop
corepack enable && pnpm install
pnpm tauri dev
```

You need Rust 1.77+ and the Tauri prerequisites for your OS
(<https://tauri.app/start/prerequisites/>).

### Command line only

Two binaries, and the one this repository is now about is the second:

```bash
cargo install --path crates/smollm-cli                # the `smollm` model CLI
smollm doctor

cargo install --path crates/agent-cli --bin smoll \
  --features openai-compatible                        # the `smoll` coding agent
smoll doctor
```

`--features openai-compatible` is not decoration: `smoll`'s default build has no
backend at all, so without it the only things that can answer a run are `mock`
and `echo`. It is the feature every release archive carries. Loading weights from
a GGUF needs `--features llama-cpp`, which compiles llama.cpp and wants cmake and
a C++ toolchain — see [providers.md](providers.md).

`./scripts/install.sh` (or `scripts\install.ps1` on Windows) builds and installs
both at once, into `~/.local/bin`, from the checkout you are standing in.

## First run

The app reads your hardware with `sysinfo` at launch and the **Home** page tells
you the largest model band that fits. Nothing else needs configuring.

1. **Models** → pick something small → **Download**. Progress, speed and
   remaining time appear on the card and in the sidebar. Closing the window does
   not cancel a download; an interrupted transfer resumes from its `.part` file.
2. Press **Load** on the finished model. The top bar pill changes from *engine
   starting* to the engine name, and the loaded model's name appears next to it.
3. **Chat** → type. Enter sends, Shift+Enter is a newline, **Stop** abandons the
   current generation mid-stream.
4. The sampling panel beside the transcript holds the seed and the stop sequences as
   well as temperature and friends: a seed makes an answer reproducible and puts that
   seed on the log line, leaving it empty draws a new one per request, and a stop
   marker ends the answer before it is shown to you. **Settings → Chat defaults** is
   what a *new* transcript starts with, and `\n` in a stop marker is a real newline —
   that is how most chat templates end.
5. Every finished answer is written to `<root>/sessions` as its own file. The
   **Conversations** panel on the right reopens, renames, searches, exports
   (Markdown or JSON) and deletes them, and the app reopens your last
   conversation when you start it again.

## Where things live

| | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Data root | `~/Library/Application Support/SmolLLM Studio` | `%APPDATA%\SmolLLM Studio` | `~/.local/share/smollm-studio` |
| Models | `<root>/models` | same | same |
| Logs | `<root>/logs` | same | same |
| Conversations | `<root>/sessions` | same | same |
| Settings | `<root>/settings.json` | same | same |

`SMOLLM_STUDIO_DATA_DIR` moves the whole root, which is how you run a portable
copy off a USB drive. **Settings → Data and privacy** has buttons that open both
folders, and an **Export diagnostics** action that writes hardware, settings and
the last log lines into one file for a bug report.

Models alone can go to another disk: **Settings → Models and engine → Move**
relocates the files and remembers the new folder, while typing a path and saving
only re-points. [models.md — moving the model
folder](models.md#moving-the-model-folder) covers what each case does to the
bytes, or `smollm models move /Volumes/fast/Models` from a terminal.

**Settings → Appearance** switches themes: the app opens in light, dark is
equally tuned, and *Follow the system* tracks the OS setting live.

## First API call

**Server** → **Start**. The page then shows the base URL, the model ids the
server actually offers on `/v1/models`, a live request log and copyable
curl, streaming and Python snippets already filled in with your port and model.

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen2.5-0.5b-instruct-gguf","messages":[{"role":"user","content":"hi"}]}'
```

Requests are served by the model resident in the engine; nothing is downloaded
or loaded on demand. Load one first — **Chat** does this for you — or ask for
`/health` to see what is there. Full reference in [api.md](api.md).

## Benchmarks

**Benchmarks** measures cold load time, prompt throughput, generation
throughput, time to first token and peak resident memory, with repetitions and a
best-of summary. It spins up its own engine instance so it never competes with
the chat. Results copy out as a Markdown `| Metric | Value |` table.

With the mock engine these numbers describe streaming overhead, not model
speed. The page says so on every result.

## Next steps

The agent's own pages, if you came for `smoll` rather than the window:

- [agent-config.md](agent-config.md) — the `smoll.toml` the loop runs on, and its overrides
- [tools.md](tools.md) — the 24 tools, their permission classes and their prompt cost
- [providers.md](providers.md) — what can answer a run, and which build each one needs
- [tui.md](tui.md) — the keys, panes and themes of `smoll chat`
- [eval.md](eval.md) — the safety promises, re-run on demand by `smoll eval`

And for both binaries:

- [release.md](release.md) — how to cut a release, and what is and is not in one
- [models.md](models.md) — the catalog, quantisations, adding your own GGUF files
- [hardware.md](hardware.md) — how the RAM-fit advice is computed
- [api.md](api.md) — the HTTP API and the Tauri command surface
- [troubleshooting.md](troubleshooting.md) — when something refuses to work
