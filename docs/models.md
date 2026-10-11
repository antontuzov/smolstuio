# Models

What this app can download, how it decides what fits, and how to bring your own
GGUF file.

## The catalog

`crates/smollm-models/data/catalog.json` is compiled into the binary
(`EMBEDDED_CATALOG_JSON`), so the Models page works with no network at all.
`schemaVersion` must be `1`; anything else is rejected rather than guessed at.

Each entry carries the coordinates needed to resolve and verify a download:

| Field | Why it is there |
| --- | --- |
| `id` | Stable key used by commands, settings and the server's `model` field |
| `hfRepo`, `filename`, `revision` | The exact blob: `https://huggingface.co/{repo}/resolve/{revision}/{filename}` |
| `parametersB`, `sizeMb`, `quantization` | Drive the RAM estimate and the size filters |
| `contextLength`, `recommendedRamGb` | What the card shows and what loading defaults to |
| `family`, `tags`, `speed`, `quality`, `vision` | Sorting, badges and recommendations |
| `architecture` | The GGUF header's `general.architecture` (`llama`, `qwen2`, `gemma3`, `phi3`, `granite`) — what an engine must support to load the file, and a Models page filter |
| `license` | Shown on the card; these models have different terms |
| `status` | `verified` or `placeholder` (see below) |

Thirteen entries ship, from 0.36B to 4B parameters, with at least one model in
every band the Models page filters by: under 1B, 1–2B, 2–3B, 3–4B and 4B and up.
`ModelCatalog::validate()` and the test suite hold that band coverage in place, so
a future edit cannot quietly empty a band. The list in
[the README](../README.md#model-support) is generated from the same file.

### `verified` versus `placeholder`

An entry is `verified` when its repository and file name were confirmed against
the Hugging Face resolve API. `placeholder` means the coordinates are plausible
but unconfirmed — the card says so, and the Models page can hide them with one
switch. Nothing is silently presented as certain when it is not.

`ModelCatalog::validate()` enforces the invariants on every load: non-empty
unique ids, `org/name` repositories, `.gguf` file names, parameter counts in
0.1–200, plausible sizes, context ≥ 512, a non-empty `architecture`, and a
buildable download URL. Problems
become warnings in diagnostics rather than a crash at startup.

## Filters

`ModelCatalog::filter()` applies search, parameter band, quantisation, tag,
license and architecture. `ModelCatalog::facets()` reports the distinct values
with entry counts, and the Models page builds every dropdown from it — so an
overlay entry with a new license or tag appears in the UI without a code change.
Whether a model is already downloaded is answered next to the library scan (in
the `list_catalog_models` command), not in this crate, so the catalog stays a
pure data structure.

## Overlays and your own models

Drop a file named `catalog.local.json` next to `settings.json` in the data
folder. Same schema, and entries merge by `id`: a matching id replaces the
bundled one, a new id is appended. That is how you add a private or unpublished
model without touching the repository.

Simpler still: **any `.gguf` file placed in the model folder shows up on the
Library page.** The scanner reads the file header directly — architecture,
quantisation, parameter count, context length, tensor count, license — so the
card is accurate even for a file the catalog has never heard of. A file whose
header cannot be parsed is listed with the parse error visible instead of being
hidden.

You do not have to find that folder yourself. **Library → Import from disk**
opens a native file panel, and dropping a `.gguf` anywhere on the window does the
same thing. Either way one `import_model` command runs the copy, and it is
validation first: the path is canonicalised, a folder or a missing file is
refused, the extension must be `.gguf`, and the header must parse before a single
gigabyte is copied. Then free space is checked, the bytes go to a
`<name>.gguf.part` staging name exactly like a download, and only after the copy
is the same length and its header re-reads clean is it renamed into place — so an
interrupted import is swept away on the next launch instead of joining the
library as a broken model.

An import never costs you a file. The source stays where it is (it is copied, not
moved), a name already held by a *different* file becomes `name-2.gguf` rather
than overwriting an installed model, and a file that already lives in the model
folder is listed without a second copy being made.

Terminal: `smollm models import ~/Downloads/MyModel-Q4_K_M.gguf`, which reaches
the same `ModelLibrary::import` as the window does.

## Verifying a file that is already in the library

A download can finish clean and still go wrong later: a disk that filled mid-copy,
a folder moved between volumes, a file edited by something else. **Library →
Verify** (or one file's **Verify** button) asks each file the questions its own
bytes can answer:

| Check | What it proves |
| --- | --- |
| File on disk | the file is there, and holds more than zero bytes |
| GGUF header | the magic, version and metadata still parse |
| Tensor data | weight bytes follow the data section start, and the file reaches at least as far as the deepest offset the header declares |
| Bytes per parameter | the weight size per parameter lands inside the range a real quantisation occupies |
| Catalog size | for a catalog file, the size still matches what Hugging Face publishes |

Each answer is `passed`, `failed` or `skipped`. Skipped is not a soft pass — it
means the file gave no number to compare against, which happens for a header
without a parameter count or a file the catalog does not know.

The **Bytes per parameter** range is 0.3–8.0, which every quantisation llama.cpp
shipped when this was written lands inside. A ternary pack below 2 bits per weight
does not: `prism-ml/Ternary-Bonsai-2-27B-gguf` measures about 0.22 and so fails
this check as a legitimate file. The card and the route for it are the last section
of [providers.md](providers.md); the short version is that the weights are fine and
the range predates them.

What no check can do is prove every weight byte survived. **GGUF stores no
per-file checksum**, so bit rot inside the tensor data is invisible from the
outside; that is also why the answer to a failed check is *delete the file and
download it again* rather than *repair it*.

Terminal: `smollm models verify` for the whole folder, `smollm models verify
SmolLM2-360M-Instruct-Q4_K_M.gguf` for one file, `--json` for scripts. The command
exits non-zero when a file fails, so it is usable from a shell.

## Moving the model folder

The folder can be re-pointed — an external SSD, a different disk, anywhere the
models fit — and the two ways to do it mean different things on purpose:

- **Settings → Models and engine → Model folder**, then Save, only re-points. The
  files stay where they were, which is what you want when the folder already holds
  them and the setting merely needs to agree.
- **Move** on that same row takes the files with it and then persists the new
  path, so the setting and the bytes cannot disagree — which is what "move my
  models" asks for, and what a plain re-point used to leave as an apparently empty
  Library.

A move is rename-first and never destructive:

| Situation | What happens |
| --- | --- |
| Both folders on one volume | the file is renamed into place — instant, no bytes re-written |
| Different volumes | free space is checked, the bytes are copied to the `.gguf.part` staging name, and the source is deleted only once the copy is proved (same length, and for a `.gguf` a header re-read) |
| The name already exists in the target | both files stay exactly where they are; equal length is reported as a duplicate, a different length as a conflict |
| A file cannot move | it stays, and is named in the report |
| Anything else in the folder | left alone — only `.gguf` and `.gguf.part` are model files |

The old folder is never deleted, and nothing in the new one is ever overwritten.
Every file ends up in exactly one of those buckets, which is what lets the app
say *what happened* rather than *it should have worked*: a paused download's
partial file moves with the folder it belongs to, and anything left behind is
listed in the note afterwards.

Two cases are refused before a single rename, because both would break live work:
a model that is loaded right now (the engine holds that file open), and a
transfer that is still running (it would resume against a folder that no longer
has its bytes). Unload or cancel first.

Terminal: `smollm models move /Volumes/fast/Models` runs the same
`ModelLibrary::relocate_from` and then writes the new path into `settings.json`,
because files that moved while the app still named the old folder would look like
an emptied library. `--json` prints the report; the command exits non-zero when a
file was left behind.

## Gated models and your token

Most of the catalog is public: pressing Download sends no credential of any kind.
A few repositories — Llama's own, some of Google's — require you to be signed in
and to have accepted their licence. Those answer `401` until the request carries a
Hugging Face access token.

Settings has a **Hugging Face access** card for exactly that: paste a
[fine-grained token](https://huggingface.co/settings/tokens) that may read gated
repositories and press **Save token**. Three things follow from how it is stored:

| What happens | Where |
| --- | --- |
| The token is written to the macOS Keychain / Windows Credential Manager | never to `settings.json` |
| Only its ends are ever shown or logged (`hf_…wxyz`) | the full value cannot be read back in the app |
| It is attached to requests aimed at the host it was saved for | a redirect to a CDN goes without it |

Removing it is the **Remove** button next to the field, and nothing else about
your setup changes. **Reset app data** leaves the credential store alone: a token
is removed when you press Remove, not as a side effect of clearing settings.

Terminal: the same store is what the CLI reads, so a token saved in the app works
for `smollm models pull` too.

```console
$ smollm auth status          # which token is live, middle hidden
$ smollm auth set             # reads the token from stdin, then stores it
$ smollm auth clear           # forgets it again
```

`auth set` takes the token from stdin rather than an argument on purpose: an
argument stays in your shell history and shows up in `ps`. Piping
(`cat token.txt | smollm auth set`) works the same as typing it at the prompt.

Where a platform has no credential store compiled into this build — Linux, and
any CI runner — the field in Settings says so and `auth set` refuses rather than
writing a plain-text file. `HF_TOKEN` (or `HUGGING_FACE_HUB_TOKEN`) is read there
instead, which keeps the token in your environment rather than on disk.

A gated failure is reported before any byte is written, and the sentence names the
fix: it says the model is gated, that a token belongs in Settings → Hugging Face
access or `smollm auth set`, and that the licence itself has to be accepted on the
model page. Once a token is saved, **Retry** on the failed transfer carries it —
the running download engine picks up the new credential without a restart.

## Downloads

A transfer is a small state machine: *queued → running → (retrying)* →
*verifying → complete*, and it can end in *failed* or *cancelled*.

**Resume.** Bytes are written to `<name>.gguf.part`, so an interrupted file is
never lost. The next attempt asks the server for the missing tail with a `Range`
request and only accepts the reply if the response is `206` and its
`Content-Range` starts exactly where the partial ends. A server that answers
`200` instead is treated as unable to resume: the partial is discarded and the
file is written from scratch, because splicing a full body onto a tail would
produce a corrupt model.

Each partial also writes a sidecar, `<name>.gguf.part.meta.json`, recording the
URL and the file `ETag` it came from. If either changed — a re-uploaded export, a
different revision, a redirect to another mirror — the old bytes are dropped
rather than stitched onto the new stream. A partial without a sidecar (written by
an older build) is kept, since the size, GGUF header and checksum checks below
still have to pass before the file is published.

**Validation before the file is trusted.** In order: the on-disk length must
match the advertised total; the first bytes must parse as a real GGUF header with
a sane tensor count (this is what rejects an HTML error page that was served with
a `200`); the SHA-256 must match when a digest is available. Only then is the
`.part` fsynced and renamed into place, and only then does it appear in the local
library — and, if the local server is running, in `/v1/models` without a restart.
A failed check deletes the partial; a half-written model never enters the
library.

**Network errors.** Transient failures — a dropped connection, `429`, `5xx`, an
unparseable `Content-Range`, a `416` the server should not have sent — are retried
automatically up to four times with capped backoff, and honour `Retry-After` when
the server sends it. The Transfers row shows *retrying (attempt 2 of 4)* in amber
instead of pretending the download died; cancel still works while the task waits
between attempts. Authoritative sizes and digests come from the Hugging Face file
metadata endpoint; when that endpoint answers nothing useful — which today it
does, `404` for files the hub still serves — the file itself is asked with a HEAD
and its own length and `ETag` are used, and only if that also fails does the
download fall back to the catalog's estimate, so a moved API path cannot refuse a
model that is plainly there. A gated repo answers `401`, and the
message says where the token belongs rather than being a bare status code — see
[Gated models and your token](#gated-models-and-your-token).

**Cancel, retry, disk.** Cancel is cooperative: the transfer loop checks a flag
between chunks, keeps the partial, and emits *cancelled*. Retry re-enters the
same path, so it resumes where it left off — and it is offered in both places a
failure is visible: the **Retry download** button on the model card and the
**Retry** button in the Transfers list. Free space is checked before the first
byte, with 64 MB of headroom, so you get `insufficient_disk_space` rather than a
full volume.

Progress events are throttled to about five per second and carry bytes, total,
percentage and the rolling speed, which is what the Transfers sidebar renders as
`MB / MB · % · MB/s`.

`smollm models pull <id>` uses the identical code path.

## Quantisation, briefly

Quantisation stores weights in fewer bits. Lower size, more loss. The catalog
sticks to Q4-class because that is where small models stay good.

| Tag | Bits/weight | Relative size | Comment |
| --- | --- | --- | --- |
| Q2_K | ~2.6 | smallest | Usually too degraded below 7B |
| Q3_K_M | ~3.4 | small | A 4B at Q3 can be worth it on 8 GB |
| Q4_K_M | ~4.6 | default | The sweet spot; what this catalog ships |
| Q5_K_M | ~5.7 | +24% | Noticeably better if RAM allows |
| Q8_0 | ~8.5 | +85% | Near-fp16; rarely worth it under 4B |
| IQ_* | varies | varies | Importance-sampled; backend support differs |

Files are named by convention (`model-Q4_K_M.gguf`), and the header records the
quantisation, so the Library page can show it truthfully.

## Backends

`Engine` is a trait; `EngineManager` holds one loaded model and hands out token
streams. Selection order is: what you asked for → what this build can actually
run → `mock`, with a warning recorded in the log and surfaced in the UI.

| Backend | Cargo feature | State |
| --- | --- | --- |
| `mock` | `mock` (default) | Streams tokens so the whole pipeline is testable. **Simulated output.** |
| `gguf-metadata` | always on | Reads headers; no generation. |
| `llama-cpp` | `llama-cpp` | **Real inference.** Loads the GGUF through llama.cpp: its own tokenizer and chat template, tokens streamed from a dedicated decode thread, cancellation checked between tokens, and metrics naming the device that actually ran. Compiles llama.cpp from the sources the `llama-cpp-2` bindings vendor, so it needs cmake and a C/C++ toolchain. |
| `candle` | `candle` | Adapter seam with a `TODO`. Experimental. |

`EngineKind` keeps those two questions apart on purpose: `compiled()` says the
feature is in the binary, `is_available()` also requires the native code to be
there, and nothing selects an engine that fails the second test. With
`--features llama-cpp` the llama.cpp engine passes both and is what answers;
without it, and for `candle` in any build, the app stays on MockEngine and says
so rather than pretending to run inference. See
[`crates/smollm-engine/src/llama.rs`](../crates/smollm-engine/src/llama.rs).
