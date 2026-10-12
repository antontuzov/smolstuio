# `smoll chat`: the same loop, drawn

```
$ smoll chat
 smoll 0.1.0  provider mock (scripted)  branch master  mode approve-edits  ctx~0/4096  tool idle  elapsed 0s  turns 0
┌ transcript ──────────────────────────────────────┐┌ plan ─────────────────────────────┐
│ . smoll 0.1.0 · /Users/me/repo · master · mode   ││ no plan yet — the scaffold runs    │
│   approve-edits · 24 tools costing 2641 tokens a ││ until the model writes one, and it │
│   step                                           ││ can call task_plan_create to       │
│ * nothing on this screen was answered by a model:││ replace it                           │
│   the mock provider replies from a script.       │├ asking about ─────────────────────┤
└──────────────────────────────────────────────────┘│ (the diff of what it wants to do)   │
┌ tool log ────────────────────────────────────────┘└───────────────────────────────────┘
│ context   3 file(s), 1180 token(s)
│ call      write_file NOTES.md
└──────────────────────────────────────────────────
› █
/help · /theme <name> · /rollback · /clear · /quit — q quits
```

`smoll chat` is `smoll task` with a screen. It builds the same `Agent` from the
same `smoll.toml`, offers the same twenty-four tools through the same sandbox
policy, asks the same approval questions, and writes the same record to
`.smoll/sessions/`. What it adds is that the transcript, the tool log, the plan
and the diff keep their places while a run goes, and that a question is answered
with one key rather than by finding the line it scrolled past.

What it does not add is a second loop. The screen is an `Observer` and an
`Approver` plugged into the loop `smoll task` uses, which is the reason the two
commands cannot disagree about what a run is allowed to do.

## The keys

| Key | Idle | Running | A question is open |
| --- | --- | --- | --- |
| any character | types into the composer | types | nothing — the keyboard belongs to the question |
| `Enter` | starts the task | refused: one run at a time | nothing |
| `Backspace`, `Ctrl+U` | edits the line | edits the line | nothing |
| `y` | — | — | yes, this once |
| `a` | — | — | yes, and for the rest of this run |
| `n` | — | — | no |
| `Esc` | says what the key would do elsewhere | stops the run | **no** |
| `q` | leaves the screen | refused: it would leave the record unwritten | nothing |
| `Ctrl+C` | leaves | leaves | leaves, and the question is answered no |

`Esc` refuses a question rather than dismissing it. A dialog that closes on `Esc`
and leaves the run going is how a UI turns "I did not look at this" into consent,
and the loop cannot tell the difference. The same rule holds on the way out: if
the screen closes while a question is open, that question is answered no, in the
same words the terminal approver uses when there is no one to ask.

`q` will not leave mid-run for the same reason: the record is written by the
worker when the turn ends, and quitting early would abandon a run that may already
have written files. `Esc` stops the run at its next check of the cancellation
token, then `q` leaves.

## Commands

`/help`, `/theme [name]`, `/tools`, `/rollback`, `/clear`, `/quit`. Anything else
starting with `/` is refused with a line saying so rather than sent to the model as
a task — a small model would faithfully try to carry out `/mode autonomous-safe`.

`/mode` is deliberately not a command. The approval mode is baked into the policy,
the tool registry and the context when the agent is built, so switching it mid-run
would mean rebuilding all three underneath a run that is going. Change it by
leaving the screen and starting it again:

```
$ smoll chat --approval-mode approve-commands
```

`/rollback` puts back the files the last run in this screen wrote, using the same
journal `smoll rollback` reads, and says how many files it restored and how many
it left alone because they had changed since the run. A run that wrote nothing is
reported as such rather than pretending to have undone something.

## What the status bar promises

Seven fields, and each one has a source that is not a guess:

- **provider** — `Agent::provider_name()`, which carries the model name when the
  provider reports one.
- **branch** — `.git/HEAD`, read as a file. No process is started to label the
  screen, and a workspace that is not a repository says `none`. A linked git
  worktree also says `none`, because its HEAD lives outside the directory; that is
  stated in `agent-repo` rather than papered over.
- **mode** — the approval mode this run was built with, which is the mode it will
  ask in.
- **`ctx~N/window`** — the tilde is the point. `N` is this repository's own token
  estimate of what the model is being shown, not a usage line from the provider,
  and it is the same estimator that decides when a prompt is too big to send. The
  window is the smaller of `[agent] max_context_tokens` and what the provider
  advertises.
- **tool** — the call the run is on, or `thinking`, or `idle`.
- **elapsed, turns** — the run's own clock and the screen's count of finished turns.

The bar turns to the warning colour at 90% of the window, because the failure a
small model runs into is not a wrong answer but a prompt that no longer fits.

## Themes

`dark` (default), `light`, `solarized`, `monokai`, chosen with `--theme` or
`/theme`. Four palettes built from ANSI colours only — no truecolour — so they
survive tmux, ssh and a terminal on sixteen colours. A palette changes what things
look like and never what they mean: a success, a refusal and a warning stay
distinct styles in every one of them, and the tests assert that.

## No screen, no run

`smoll chat` needs a terminal on both stdin and stdout. In a pipe, a CI job, or
under `--json`, it says so and exits 2, pointing at `smoll task`, which runs the
identical loop as printed output. It does not decide to draw anyway: escape
sequences written into a file someone meant to read is not a fallback, and a
process that takes over a terminal it was not given can leave a shell that does not
echo.

Raw mode and the alternate screen are undone on every path out — a clean quit, a
draw that failed, a panic inside a widget. A panic hook is installed for the last
of those, so a crash leaves a readable message on a normal screen rather than a
wall of escape codes.

## What is deferred, on purpose

- **No streaming text.** The loop does not emit `AgentEvent::Delta` yet, because
  no engine adapter streams into it; a provider that can stream says so in its
  capabilities and the answer still arrives whole. The screen is ready for deltas
  (they join the line they belong to) and nothing more.
- **No scrolling.** Each pane keeps its newest rows up to a cap — 400 transcript,
  200 log, 300 diff — and the row that does not fit is the oldest one. A run that
  goes twenty steps is read from the record afterwards with `smoll review`, which
  has the whole thing.
- **No syntax highlighting.** The diff pane colours `+`, `-` and `@@` lines and
  leaves the rest plain. A highlighter is another dependency and another way to be
  wrong about a file's language.
- **No command palette, no mouse, no splits.** Keys are listed above and the hints
  line names them on screen.
- **No `/mode`.** See above.

## How this was checked

- Unit tests in `crates/agent-tui`: the state model folds every `AgentEvent` into
  the right pane; `press()` is tested key by key, including that `Esc` refuses a
  question and that a question owns the keyboard; the layout maths is asserted row
  by row, which is where the wrapping and the newest-row rules live.
- `crates/agent-tui/tests/screen.rs` draws a real frame into `ratatui`'s
  `TestBackend` and reads that buffer back cell by cell, so the claim is about the
  cells a terminal would receive rather than about the code that produced them: the
  status bar as the first row with all of its fields, every pane's title, a
  question with all three of its answers unclipped, the newest rows of a 120-line
  run and the oldest ones gone, the plan keeping its rows from the top while the
  run goes on, a theme that changes the colours and not one character, and
  terminals too small to hold every pane. This is the file CI runs instead of a
  screen.
- The binary over a real pty: `smoll chat` was driven against a loopback
  OpenAI-compatible server that scripts one `write_file` call. The approval strip
  appeared, `y` let the write through and the file landed, `n` and `Esc` both
  refused and wrote nothing, `Esc` mid-request stopped the run and the line after
  it said so, and `/help`, `/theme`, `/tools`, `/clear` and `/rollback` each drew
  what they promise. Exit codes came out 0 for the accepted run and 1 for the
  refused and stopped ones. The branch in the bar came from a `.git/HEAD` file on
  disk, with no git process started to produce it.
- `cargo test -p agent-cli` covers the two refusals a script can reach: `chat` in
  a pipe, and `chat --json`.

The harness that drove the pty lives outside the repository on purpose: CI has no
terminal, and a test that cannot run there is a tool, not a gate.

## Dependencies, and why they are pinned where they are

`ratatui` is held at 0.29 because 0.30 requires rustc 1.88 and this workspace's
floor is 1.77; a person on last year's distribution toolchain can still build the
binary. `crossterm` is pinned to the line ratatui 0.29 itself requires, because a
second copy in the tree would mean the event loop and the renderer holding two
different handles on one terminal.
