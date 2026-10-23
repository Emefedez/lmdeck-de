# Prism ternary GGUFs (PQ2_0 / PTQ1_0)

Some ternary GGUFs use a packing that mainline llama.cpp does not read. lmdeck can keep the stock build for
everything else and use a second, Prism-capable build for those files. This page is the plan we followed, what was
measured, how the automatic choice works, and how to set it up or back out.

Short version: point `llama.prism_path` at a `PrismML-Eng/llama.cpp` build (branch `prism`) once. Models whose
header needs it then load automatically, marked `prism` in Models and in the bar panel; everything else stays on the
stock build. A per-model **Engine** setting can force either one.

## The files

A ternary model stores each weight as -1, 0 or +1. The file name says how the trits are packed:

| File | ggml type | Groups of | Runs on |
|---|---|---|---|
| `*-Q2_0.gguf` | 42 | 64 | stock llama.cpp and the Prism fork |
| `*-PQ2_0.gguf` | 142 (`GGML_TYPE_PQ2_0`) | 128 | only the Prism fork |
| `*-PTQ1_0.gguf` | 143 (`GGML_TYPE_PTQ1_0`) | 128 | only the Prism fork |

Prism's files are also **hadamard-folded**: the weights are stored in a rotated basis, and the header carries
`prism.hadamard.*` metadata - the block size (1024 in the file measured here), the per-input sign vector and the GDN
feature permutation (401 weights for that model). A loader that knows type 142 but not the sign vectors decodes the
weights without error and produces garbage, so the metadata is not optional.

Example: `huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:...-Ternary-Bonsai-PQ2_0` (7.2 GB, 27B). Its header:

- `general.file_type` = 141, but the highest tensor type is 142 (`max_type`);
- 348 tensors of type 142, plus F32/BF16 norms, Q3_K/Q2_K tensors;
- `prism.hadamard.*` keys.

The fork itself adds `GGML_TYPE_PQ2_0 = 142`, `GGML_TYPE_PTQ1_0 = 143` and their file types
(`GGML_FTYPE_MOSTLY_PQ2_0 = 128`, `GGML_FTYPE_MOSTLY_PTQ1_0 = 129`) in `ggml/include/ggml.h`.

## Why the stock build cannot load them

Stock llama.cpp (`c35b667`, the build in `~/.local/share/lmdeck/llama.cpp`) knows types 0-42
(`GGML_TYPE_COUNT = 43`; 41 = `Q1_0`, 42 = `Q2_0`). Loading the Huihui file fails immediately with:

```
tensor 'output.weight' has invalid ggml type 142. should be in [0, 43)
```

Upstream has the activation hint the fork uses (`GGML_HINT_SRC0_IS_HADAMARD`) but not the types. The tracking issue
[ggml-org/llama.cpp#29058](https://github.com/ggml-org/llama.cpp/issues/29058) is still open.

### The upstream PR we did not port

[PR #29077](https://github.com/ggml-org/llama.cpp/pull/29077) implements the hadamard folding for the **CPU
backend only**: 16 files, about +737 lines (`ggml-cpu/ops.cpp`, `quants.c`, `llama-graph.cpp`, loader and model
plumbing). It was closed unmerged. Measured here at roughly **0.4 tok/s** decode on the 27B - unusable next to the
6.8 tok/s the Vulkan fork gets. Porting it would also have meant maintaining a patch on our own build, so lmdeck
does not carry it.

What it did confirm is the shape of the change: `llama_mul_mat_hadamard()` applies the forward rotation to the
input before the matmul (memoised per weight) and the inverse after, driven by the loader's
`hadamard_rotations`/`hadamard_inverses` maps. The fork does the same on the GPU.

## The fork

`https://github.com/PrismML-Eng/llama.cpp`, branch `prism` (at `73bfe6c`, 2026-10-19, built here with Vulkan on).
It tracks mainline, so ordinary GGUFs keep working through it. Its Vulkan backend adds:

- `fwht.comp` (136 lines): fast Walsh-Hadamard transform, forward and inverse, with the stored signs;
- `dequant_pq2_0.comp`, `dequant_ptq1_0.comp`;
- `mul_mat_vec_ptq1_0.comp`, `mul_mat_vecq_ptq1_0.comp`, `ptq1_0.glsl`.

The fork's README prefers `*-PQ2_0.gguf` on Metal/CUDA/HIP/CPU; legacy group-128 files that reuse id 42 need its
`prism-v5`/`prism-b9601` tags. Our stock build and the fork share the same CLI flags, so lmdeck launches both
identically (per-process `HK_SYSMEM` GPU heap, `-fa auto`, `-np 1`, `--jinja`, cache reuse, `-ngl 999`).

### Measured

All through lmdeck's own launcher, one stream, greedy:

| Model | Build | Prompt | Decode | Load |
|---|---|---|---|---|
| Huihui Qwen3.8-27B Ternary-Bonsai-PQ2_0, 7.2 GB | Prism `73bfe6c`, Vulkan | 37.3 tok/s (519-token prompt) | 6.8 tok/s | 5.6 s (lmdeck) |
| Qwen3.8-27B-OBLITERATED Q2_K, 10.1 GB | Prism `73bfe6c` | 13.3 tok/s | 4.9 tok/s | |
| same Q2_K file, same flags | stock `c35b667` | 13.5 tok/s | 4.9 tok/s | 6.8 s (lmdeck) |

The Q2_K prompt figures come from the A/B script's ~30-token prompt, so they are dominated by fixed startup cost;
the Huihui figure is a real 519-token prompt. The A/B says the fork imposes no measurable slowdown on a normal
model: within noise on both prompt and decode. On the Huihui file, all 130 tensors go to `Vulkan0` with 2 graph splits and the answers are coherent (`17*23` -> 391).

6.8 tok/s is not a fast daily driver: the 4-bit MLX version of the same model runs at 11-13 tok/s. The point of the
ternary file is memory - 7.2 GB of weights instead of 15.0 GB - so it leaves room for a long context or another
model. Treat it as a fallback, not the default.

## How lmdeck chooses the build

A second binary, not a second install path: `llama.prism_path` (Settings, section GGUF, or
`lmdeck config llama.prism_path <path>`). The stock build stays at `~/.local/share/lmdeck/llama.cpp/bin/llama-server`
(or `llama.server_path`), the Prism build at `~/.local/share/lmdeck/llama-prism/bin/llama-server`.

Resolution order for a GGUF:

1. the per-model override, `[overrides."<model>"].engine = "stock" | "prism"` (Models -> `e` -> **Engine**);
2. automatic: the file needs Prism (hadamard metadata, or a highest type above 42 up to 143) and `llama.prism_path`
   is set and exists -> the Prism build;
3. `llama.server_path`, then lmdeck's managed build, then `llama-server` on `$PATH`.

The header check is done once per scan (`GgufMeta.file_type`, `max_type`, `prism`; `needs_prism()`), so nothing is
probed at load time beyond the binary. When `llama.prism_path` points at a missing file and a model wants it, the
load fails with that path in the message instead of the upstream "invalid ggml type 142".

The choice is visible in three places:

- **Models** (TUI): a cyan `prism` next to the name (or a red `✗ needs Prism build` if none is configured), an
  **Engine** line in the details pane, and the **Engine** field in the model settings popup (auto / stock / prism,
  Enter cycles, `x` clears back to auto).
- **Bar panel**: `prism ·` before the format/quant/size meta line, and the load tooltip says which build runs it.
- **API**: `GET /lmdeck/models` adds `needs_prism` (header), `engine` (`stock` or `prism`, the effective build) and
  `engine_choice` (`auto`, `stock` or `prism`). Per-model override:
  `POST /lmdeck/override {"model": "...", "key": "engine", "value": "auto|stock|prism"}`.

`lmdeck ls` no longer lists a Prism file under "Cannot load:" once a Prism build is configured; if none is, the
reason says to set `llama.prism_path` (or to rebuild the managed llama.cpp from the fork).

## Setup

### A. Second build (what is installed here)

```
git clone --branch prism https://github.com/PrismML-Eng/llama.cpp \
  ~/.local/share/lmdeck/llama-prism/src
cmake -S ~/.local/share/lmdeck/llama-prism/src -B ~/.local/share/lmdeck/llama-prism/build \
  -DGGML_VULKAN=ON -DCMAKE_BUILD_TYPE=Release -G Ninja
cmake --build ~/.local/share/lmdeck/llama-prism/build -j
mkdir -p ~/.local/share/lmdeck/llama-prism/bin
cp ~/.local/share/lmdeck/llama-prism/build/bin/llama-server ~/.local/share/lmdeck/llama-prism/bin/
lmdeck config llama.prism_path ~/.local/share/lmdeck/llama-prism/bin/llama-server
```

Disk: 1.6 GB including the source and build trees; the binary is 76 MB. Nothing restarts on the config change - the
next load reads it (models already loaded are marked stale).

### B. The managed build, fork instead of stock

Only one llama.cpp, the fork:

```
lmdeck config llama.repo https://github.com/PrismML-Eng/llama.cpp
lmdeck config llama.git_ref prism
lmdeck llama build
```

This loses the ability to keep the stock build for ordinary models, which the A/B says costs nothing - but it also
means every future `lmdeck llama update` follows the fork. Route A is the low-risk one.

## Backing out

```
lmdeck config llama.prism_path ""
```

Every model goes back to `llama.server_path` / the managed build. The old interim workaround of setting
`llama.server_path` to the Prism binary can stay as a fallback but is no longer needed; `llama.server_path` is
empty here now. To reclaim the disk, remove `~/.local/share/lmdeck/llama-prism`.

## Open questions / next

- Upstream support, when it lands with GPU kernels, removes the second binary; [PR
  #29672](https://github.com/ggml-org/llama.cpp/pull/29672) is still open at the time of writing.
- The fork tracks mainline; rebuild it like llama.cpp if a new model architecture is needed. `llama.repo` +
  `llama.git_ref` also lets **Backends -> Build llama.cpp** produce a fork-only install if that ever becomes
  preferable.
- The ternary Bonsai file quantizes the same 27B model much smaller than 4-bit MLX, but at roughly half the decode
  speed here. It is a memory saver, not a speed win.
