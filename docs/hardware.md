# Hardware

How the app decides what your machine can run, and why it says what it says.

## What is measured

`smollm-hardware` reads the machine once at launch with `sysinfo` (and a little
platform probing), then caches it. The Home page has a **Re-measure** button
because free RAM changes while you work.

| Field | Source |
| --- | --- |
| `platform`, `arch` | `std::env::consts` |
| `cpuBrand`, `physicalCores`, `logicalCores` | `sysinfo::System` |
| `totalRamGb`, `availableRamGb` | `sysinfo` memory |
| `diskFreeGb`, `modelVolumeFreeGb` | statvfs on the data volume |
| `appleSilicon`, `metalAvailable` | CPU brand + `arch` (`aarch64` on macOS); Metal is assumed on any Mac |
| `nvidiaGpu` | `nvidia-smi --query-gpu=name` when it is on PATH |
| `vulkanAvailable` | `vulkaninfo --summary` succeeds |
| `gpuName` | Best available description, e.g. `Apple M3 Pro` |
| `accelerator` | The engine's own answer: llama.cpp's device list, asked after `sysinfo` is done, `null` in a build with no native engine |

Nothing is uploaded, and no measurement leaves the process. Asking llama.cpp
initialises it, so the question is asked on the blocking pool and the answer is
cached with the rest of the report.

## The RAM estimate

Loading a GGUF file needs more than the file size: weights, plus a KV cache that
grows with context, plus runtime overhead.

**When the file is on disk, the app measures instead of guessing.** A GGUF header
says where its tensor data begins, so the weights are the bytes after that point;
and it says how many layers and key/value heads the model has, so the KV cache is
sized by the real attention geometry rather than a constant per token:

```
head_dim     = embedding_length / head_count
kv_cache     = 2 × context × layers × kv_heads × head_dim × 2 bytes
ram_needed   = weights + kv_cache + ~350 MB of runtime
```

A 360M model with 256 MB of weights at 8K context measures 0.25 + 0.31 + 0.35 ≈
0.9 GB. Note that grouped-query attention makes this much smaller than the
heuristic below, which is why a model can move from "will not fit" to "fits".

The formula does still assume full attention on every layer. A hybrid model — most
of whose layers keep a fixed-size recurrent state instead of a cache that grows
with context — is charged for a cache it never builds, so the estimate is an upper
bound rather than its need. The worked example, including the 53 GiB this
arithmetic produces for a 27B file that runs in a server on a much smaller
machine, is the last section of [providers.md](providers.md).

**For a model that is not downloaded yet** there is nothing to read, so the
coarse figure stands:

```
weights_gb   = sizeMb / 1000
kv_cache_gb  = contextLength × 0.00012
ram_needed   = weights_gb + kv_cache_gb + 0.35
```

So a 491 MB model at 32K context asks for about 0.49 + 3.9 + 0.35 ≈ 4.7 GB. That
is why the app sometimes says a 500 MB file needs 5 GB before you download it,
and why dropping the context length is the cheapest way to fit a bigger model.

Only 70% of *currently free* RAM counts as usable:

```
usable_ram_gb = max(availableRamGb × 0.7, 0)
fits          = ram_needed ≤ usable_ram_gb
```

The headroom exists because macOS and Windows will start swapping at exactly the
point where the arithmetic says it fits, and a swapping laptop is a bad
demonstration of a small model.

## Recommendations and bands

`build_doctor()` walks the catalog, keeps what fits, and reports the largest
parameter band that does — capped at 4B, because this app is about small models
and it would be dishonest to imply that a 32 GB machine should run 40B through a
UI built for laptops.

| Band | Headline you get |
| --- | --- |
| ≥ 3B | "Your machine looks great for 0.5B–4B models." |
| ≥ 1.7B | "Your machine can comfortably run 0.5B–1.7B models." |
| below that | "N GB RAM detected. Recommended: 0.5B and 1B models with Q4_K_M quantization." |

The detail line adapts to the accelerator it found: Apple Silicon unified memory,
an NVIDIA GPU (where CUDA beats CPU on 2B–3B), a Vulkan adapter, or nothing —
in which case it says CPU is the safe choice and that small models stay
responsive anyway.

## Warnings

The doctor adds warnings rather than refusing:

- Total RAM under 6 GB → close other apps, prefer 0.5B.
- Free RAM below 35% of total → free memory before loading something larger.
- Under 5 GB free on the model volume → downloads will start failing.
- Free RAM below 50% of total → a note appended to the detail line.

## Backend choice

```
macOS + Metal available        → metal
Windows/Linux + NVIDIA GPU     → cuda
Windows/Linux + Vulkan loader  → vulkan
otherwise                      → cpu
```

The chosen backend is a *recommendation*: it becomes the default in Settings, and
you can override it there. A backend only helps if the build contains an engine
that can run on it. `llama-cpp` is a cargo feature that compiles and links
llama.cpp, so a build made with `--features llama-cpp` really does offload layers
to Metal, CUDA or Vulkan; `candle` compiles an adapter without a runner behind
it. In a build with neither, all four backends resolve to MockEngine.

Nothing is inferred from the settings once a model is loaded: llama.cpp is asked
which device it got, and that is what metrics, the chat page and the diagnostics
export report. If it says the weights stayed on the CPU, the app says CPU.
Whatever is substituted, `EngineManager` falls back to something that works and
logs it, and the UI shows a warning toast saying which engine actually answered
rather than hiding the swap.

The same device list answers a second question: the memory llama.cpp allows
itself on that device. Even on unified Apple Silicon it is a capped slice of RAM
(11.8 GiB of 16 GiB on the M1 this is written on), so `smollm hardware` prints it
as the `Offload` line and the Home page shows it under *This machine* rather than
implying the GPU can reach all of memory.

## Decode threads

Settings → *Decode threads* is the number llama.cpp may use to decode, and it is
per-machine rather than per-model: `1` up to `64`, or left unset to use every core.
Leaving it unset is the right answer most of the time — a model whose layers are all
offloaded to a GPU is not decoding on the CPU in the first place, so extra threads
buy nothing. Ask for more threads than `available_parallelism()` reports and the
load still succeeds, but on the count the machine can actually fill, with a warning
naming both numbers; that is deliberately a warning rather than a silent clamp,
because a thread count is something you chose. `smollm run --threads N` overrides it
for one request, and a benchmark measures the machine as it is configured.

## Practical guidance

| You have | Do this |
| --- | --- |
| 8 GB laptop | 0.5B–1.5B at Q4_K_M, context 4K–8K |
| 16 GB | 3B fits; keep context under 16K |
| 32 GB+ | 4B is still the recommendation; longer contexts become affordable |
| Intel Mac | CPU inference; stay at or below 1.5B |
| Windows + GTX/RTX | CUDA once a native backend is linked; CPU today |
| Only integrated graphics | CPU is fine for this size class |
