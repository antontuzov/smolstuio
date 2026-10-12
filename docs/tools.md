# The tools a run can be given

Twenty-four tools, in four classes, each with its arguments written down and its
price measured. `smoll tools` prints this list for the build you are holding,
with the ones your configuration switched off still shown and marked `off`, and
`smoll tools --json` gives the same rows to a script.

Every number below was measured on this machine against the definitions in
`crates/agent-tools/src`, not estimated. `smoll eval` re-runs the measurement and
`smoll tools` prints the same totals for the build you are holding, so none of
them is a figure you have to take on trust — see
[docs/eval.md](eval.md) for what each command counts. The cost column is a tool's
`ToolSchema::token_cost` — roughly how many prompt tokens offering that tool
spends **on every step of every run**. The list is the largest fixed cost a small
model pays, which is why these tools take as few arguments, and as few words, as
they can.

A description here is not marketing copy, and shortening one has a floor: each
keeps **what the answer is** (a diff, an exit code plus output, `file:line:text`),
**the one clause that stops a wrong call** (`run_command` starts no shell,
`search_text` is not a regular expression, a patch's context must match exactly,
`git_add` refuses `.`), and **what a person needs before approving it** (`smoll
rollback` does not undo a commit; `delete_file` restores only a text file within
the read limit). Word that survives those three is the word a model would have
had to spend a step rediscovering.

## The table

| Tool | Class | Timeout | Output cap | Tokens/step | What it does |
| --- | --- | --- | --- | --- | --- |
| `read_file` | read-only | 20s | 8192 B | 135 | one file, at most a few hundred lines |
| `list_dir` | read-only | 20s | 8192 B | 106 | one directory's entries |
| `search_files` | read-only | 20s | 8192 B | 124 | file names against a glob |
| `search_text` | read-only | 60s | 8192 B | 178 | a literal string inside files |
| `search_symbols` | read-only | 60s | 8192 B | 159 | where a name is defined |
| `git_status` | read-only | 20s | 8192 B | 94 | what the working tree holds |
| `git_diff` | read-only | 20s | 8192 B | 139 | the change, as a diff |
| `git_log` | read-only | 20s | 8192 B | 122 | recent history, 100 commits at most |
| `git_show` | read-only | 20s | 8192 B | 112 | one commit |
| `memory_recall` | read-only | 20s | 8192 B | 76 | what earlier runs kept |
| `task_plan_create` | read-only | 20s | 8192 B | 110 | the model's own plan, in place of the scaffold |
| `task_plan_update` | read-only | 20s | 8192 B | 117 | the same plan, rewritten part-way through |
| `write_file` | write | 20s | 8192 B | 127 | replace a whole file |
| `patch_file` | write | 20s | 8192 B | 125 | apply a unified diff |
| `memory_store` | write | 20s | 8192 B | 86 | keep one sentence for the next run |
| `git_add` | write | 20s | 16384 B | 120 | stage the files it names |
| `git_commit` | write | 120s | 8192 B | 116 | record what is staged |
| `delete_file` | destructive | 20s | 8192 B | 101 | remove one file, if the journal can restore it |
| `run_command` | execute | 300s | 8192 B | 165 | one program, named by the caller |
| `test_run` | execute | 300s | 8192 B | 70 | the project's test command |
| `check_run` | execute | 300s | 8192 B | 71 | the project's compile check |
| `lint_run` | execute | 120s | 8192 B | 62 | the project's linter |
| `build_run` | execute | 300s | 8192 B | 59 | the project's build |
| `format_run` | execute | 60s | 8192 B | 67 | what the formatter would change, reporting only |

**2641 tokens a step for all twenty-four.** Twelve read, five write, one deletes,
six run a program. The classes are the sandbox's vocabulary, and
`smoll tools` prints them because a person should not have to guess which of
these can remove a file.

The whole list used to cost 3217. Every tool is still here, and every clause a
guard test insists on is still here; what changed is the number of words spent on
each one, and 576 tokens a step is what the smallest model was given back for the
file it actually asked to read.

## What the classes decide

The class is not a label; it decides whether anything happens without you.

- **read-only** runs on its own in every approval mode. Nothing it can do changes
  the tree, so there is nothing to ask about.
- **write** asks first in the default mode (`approve-edits`) and, when it asks,
  shows you the diff or the sentence it means to keep. In `suggest-only` it is
  blocked before the tool is entered, which is what makes that mode's promise
  true rather than a description of what usually happens.
- **execute** runs on its own in the default mode and asks first in
  `approve-commands`.
- **destructive** never runs without an answer: it is the one class no approval
  mode lets through on its own, and `suggest-only` blocks it outright. A deletion
  is asked about even when writes are free, because "this cannot be undone by
  writing the file back" is a different question.

The sandbox is a second, independent gate: `strict` blocks an action that crosses
a boundary (a path outside the workspace, a `git reset --hard`-shaped command, a
denylist hit, an allowlist miss), `warn` records it and runs, `off` records it
without a verdict. It is not the same axis as the class — a read-only tool with a
path outside the root is still a violation.

A tool declares its class and the sandbox can only raise it. `run_command`
declares execute and says it takes a program; when the model asks for
`rm build.rs`, the argv is classified as **destructive** before the policy is
consulted. That is what makes a declared class a floor rather than an answer —
a persuasive file name cannot talk a command tool down into a quieter class.

## Where a command comes from

This is the distinction that matters most in the table, and it is why six tools
run programs instead of one.

`run_command` takes its argv **from the model**, as a list. No shell is started,
so redirection, pipes, `&&` and `;` are not features a model can reach — they
become literal arguments the program rejects. A bare string is accepted and split
on whitespace, because a 1.5B model writes that more reliably than the array, but
splitting is not handing it to a shell. The working directory has to be inside the
workspace, and so does a program named by path. Prefix allowlists and denylists
under `[tools.run_command]` are checked against the joined command.

`test_run`, `check_run`, `lint_run`, `format_run` and `build_run` take **no
arguments at all**. Their argv is read off the repository by `agent_repo::Project`
— `Cargo.toml`, `package.json` or `Makefile` — and a model cannot add a flag to
it, so the test tool cannot be talked into wiping `target`. A project that
declares no test command is reported as such rather than guessed at with a
plausible one. `format_run` only asks the formatter to report what it would
change, and refuses a command it cannot put in report mode — a `make fmt` target,
a fixing linter — because a rewrite is not what a check asks for.

The six git tools work the same way as the runners: the argv is written out in
full in `git.rs`, and the model supplies a path, a limit, a revision or a commit
message. A revision is refused unless it has one commit's shape (`HEAD~1`, a sha,
a branch name); anything starting with a dash, anything with a space and anything
with an equals sign are rejected as options rather than names. `git_add` refuses
to stage the whole repository — it wants the paths named. Because the argv is
fixed, the four readers can honestly be read-only and the two writers writes,
which a command-derived intent would have raised to execute. Nothing in the git
set names a remote: no push, no fetch, no branch created or deleted, and
`smoll rollback` does not undo a commit.

## Where a path goes

The readers walk with the `ignore` crate, so `.gitignore` and `.agentignore` are
honoured before a file is opened — a build directory holds a copy of every string
in the project, and a search that returned it would be a search that returned
nothing useful. Every path is resolved inside the workspace root; one that leaves
it is refused rather than sandboxed loosely. The three search tools are literal,
not regular expressions: a small model that writes a pattern with parentheses
means something and gets a message about an unusable pattern, while one that
writes a word means the word.

Writers go through `agent_repo::patch`, so the path rules, the context matching
and the snapshot are the same code everywhere, and each leaves what it was about
to overwrite in the rollback journal. `delete_file` refuses anything the journal
could not restore. The registry asks the policy before any of them is entered, so
in `suggest-only` nothing is written and the diff is the deliverable.

## The two that are not about the repository

`memory_store` appends one short sentence to `.smoll/memory.jsonl`, and the next
run's first turn already contains it. It is a write — you see the sentence before
it is kept — and its content is run through the same redaction the audit log uses
**on the way in**, so a credential never exists in the file. It is deliberately
outside the rollback journal, like a git commit: `smoll rollback` puts the tree
back and leaves what the run learned alone, because forgetting is not what undoing
a change means. `smoll doctor` counts the notes.

`task_plan_create` and `task_plan_update` are read-only because a plan is not a
file: they write nothing, and an agent should not have to be approved to think.
The loop opens every run with a four-step scaffold it wrote from the repository;
these two let a model that knows better replace it, up to eight steps of ninety
characters each. A longer plan is refused rather than truncated — dropping "run
the tests" is not a safe way to fit a budget. Once the model owns the plan the
loop stops guessing which sentence a read satisfied, but it does not stop checking
the work: the project's validation command still runs, and it ticks a box only if
the new plan still names the command. Copying the block back with an `x` in the
brackets is how a step gets marked done; the characters the prompt renders are the
characters the tool parses, on purpose.

## Turning tools off

The list is the part of the prompt a person can trim, and the configuration is the
way:

```toml
[tools.git_show]
enabled = false
```

Eight switched off (`git_status`, `git_diff`, `git_log`, `git_show`,
`patch_file`, `task_plan_update`, `memory_recall`, `check_run`) leaves 16 offered
and 1785 tokens a step instead of 2641. `smoll tools` prints the current number,
and a run that cannot fit its model's window stops with `context tokens needed`
and names this command in the hint under it, rather than trimming the repository
down to nothing.

One measured consequence worth knowing before a twenty-fifth tool is added: on a
two-file repository, the first turn puts 3578 tokens in front of a model that is
handed the tools as a native block and 1751 in front of one that has to be told
them in prose — one sentence per tool where the block is a schema per tool. Both
numbers are inside a 4096-token window now, so both models get the rules, the
whole list, the plan, the repo map **and the bodies of the files in it**. They
were counted as bytes on the wire, from a real OpenAI-compatible server, which
includes the JSON each schema is wrapped in; `smoll eval` prints the same two
first turns counted the way the loop counts them — 3210 and 1727 — because that
is the arithmetic it uses to decide whether to ask at all. Shortening
the definitions is what bought that back; before it the native block cost enough
at 4096 that the model was given the map and no file contents at all, and had to
ask for the file it wanted with `read_file`. The window where the list still fits
and a file body does not is 3584, which is a row in `smoll eval`'s table rather
than a claim in this one, and
`a_window_too_small_for_the_tool_list_runs_on_the_map_instead_of_bodies` in
`crates/agent-core/tests/loop_runs.rs` asserts exactly that: the map names the
files, none of them is handed over, all twenty-four tools are still offered, and
the run reads what it needs and finishes without a warning.

## What is not here

No tool reaches the network, so the `Network` class has no member in this build,
and no tool pushes, fetches, merges, rebases or deletes a branch.

Seven of the brief's named tools are not implemented, and each is missing for a
reason rather than by oversight:

- `explain_code`, `propose_refactor` and `review_diff` are the loop's job with
  `read_file`, `search_symbols` and `git_diff`. Making them tools would give a
  model a second, narrower prompt to satisfy for an answer the same three readers
  already support, and a name that sounds like a safety guarantee it cannot give.
- `generate_tests` is a write with an opinion: the only honest version of it is
  `write_file` on a test file, and the run that writes a test should read the code
  it tests first — which is what the loop's rules already make it do.
- `dependency_audit` wants an advisory database. `npm audit` and `cargo deny` both
  reach a registry, and a local answer with no cache would be a guess dressed as a
  finding. `smoll` runs the project's own commands instead, so if a repository
  declares `audit` as a script its author can wire it through `run_command`.
- `git_branch` is a thing a person decides about their repository. A
  name-shaped argv tool can be talked into a deletion by a model that has only
  ever seen a branch name, and the branch a run creates is work the review has to
  understand twice.
- `mcp_call` is not here, so a run cannot be handed a server's tools. That is the
  one gap that will matter most to someone reading this list, and it stays open
  until there is an answer to what a tool from outside the repository does to the
  approval model above.
