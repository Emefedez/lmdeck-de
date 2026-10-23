# Models

Which models run depends on the backend, its version and your free memory, so lmdeck checks rather than relying on
a fixed list. This page says how it checks, and which models have actually been run here.

## How lmdeck tells you before you download

- **Get models** shows each variant's estimated memory (weights plus the attention cache for your context settings
  plus runtime overhead) and whether it fits now: fits, tight (needs a raised GPU heap and the free memory for it),
  or too big.
- **MLX compatibility markers**: ✓ qualified by the omarchy-mlx project on this backend (its catalog, shown under
  Get models), ? untested, ✗ will not run. A checkpoint that is not a standalone chat model (a speculative draft
  such as DFlash/MTP, a classifier) or whose `model_type` has no implementation in the installed `mlx_lm` is marked
  ✗ unsupported in search and Models, with the reason in the details pane. lmdeck probes which quantization modes the
  installed runtime can run on the GPU, and gives a ✗ with a recommended alternative where one is known.
- **Models** grades what you have downloaded (see the README's grader section).

## By backend

| Backend | Runs | Does not run |
|---|---|---|
| MLX (omarchy-mlx 0.7.28 package) | MLX safetensors with affine quantization (4/8-bit `mlx-community` repos), dense or hybrid (Qwen3.5/3.6/3.8 attention + linear layers), mixture of experts | nvfp4, mxfp4 and mxfp8 quantization (no GPU kernels); models with ops the backend lacks (lmdeck offers the CPU, much slower) |
| GGUF (llama.cpp, Vulkan) | What llama.cpp supports. Partial GPU offload when a model does not fit whole (`llama.auto_offload`) | Prism's PQ2_0/PTQ1_0 ternary types on the stock build (see below) |
| Strata (strata-asahi) | Qwen3.8-Flash-Next packs set up by Strata's setup (`strata-<model>.json`) | Any other model: Strata runs this one model family |

The most memory a model can have depends on what else runs. On this 32 GB machine, with the desktop running,
lmdeck admits MLX models up to about 19 GiB of weights, for example a 35B-A3B at 4-bit. It needs about 23 GiB free
to do so (use **Make room** first).

## Ternary GGUFs (Bonsai)

A ternary model stores each weight as -1, 0 or +1, and the file name says which packing it uses:

| File | ggml type | Runs on |
|---|---|---|
| `*-Q2_0.gguf`, `*-Q2_0_g64.gguf` | 42, group 64 | stock llama.cpp (and the Prism fork) |
| `*-PQ2_0.gguf` | 142, group 128 | only the Prism fork: `PrismML-Eng/llama.cpp`, ref `prism` |
| `*-PTQ1_0.gguf` | 143, dense trits | only the Prism fork |

Prism's files are also *hadamard-folded*: the weights are stored in a rotated basis and the header carries
`prism.hadamard.*` (block size, per-input sign vector, the GDN feature permutation). A loader that reads the type but
does not undo that transform decodes the weights correctly and still produces garbage, so the metadata is not
optional. Upstream llama.cpp has the activation hint (`GGML_HINT_SRC0_IS_HADAMARD`) and `Q2_0` group 64, but not
these two types ([#29058](https://github.com/ggml-org/llama.cpp/issues/29058) is still open).

lmdeck reads every GGUF header before you load it. A file whose highest tensor type is above what the installed
build knows, or that carries `prism.hadamard.*`, is marked ✗ with the reason in Models, and `lmdeck ls` prints it
under "Cannot load:":

```
Cannot load:
  huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:...-Ternary-Bonsai-PQ2_0: Prism rotated ternary
  (custom ggml type 142, file type 141): needs a Prism-capable llama.cpp - set llama.repo =
  https://github.com/PrismML-Eng/llama.cpp and llama.git_ref = prism, then build it
```

To run those files, build the fork instead of upstream (it tracks mainline, so ordinary GGUFs keep working):

```
lmdeck config llama.repo https://github.com/PrismML-Eng/llama.cpp
lmdeck config llama.git_ref prism
lmdeck llama build          # Vulkan; the daemon picks it up on restart
```

Or keep the stock build and point at an existing Prism build; lmdeck launches it exactly like its own, so the
Honeykrisp changes (per-process `HK_SYSMEM` heap, `-fa auto`, `-np 1`, `--jinja`, cache reuse) still apply:

```
lmdeck config llama.server_path ~/.local/share/lmdeck/llama-prism/bin/llama-server
```

The fork's Vulkan backend has FWHT (Hadamard) pipelines and PQ2_0/PTQ1_0 matmuls, so the transform runs on the GPU.
Verified here with `PrismML-Eng/llama.cpp` at `73bfe6c` (branch `prism`, 2026-10-19): the Huihui PQ2_0 file loads in
about 5 s, all 130 tensors go to Vulkan (2 graph splits) and the answers are coherent. Speed is 6.8 tok/s decode and
37 tok/s prompt on a 519-token request - slower than the 4-bit MLX version of the same model (11-13 tok/s) but at half
the memory (7.2 GB).

## Run on this machine

| Model | Backend | Result |
|---|---|---|
| mlx-community/Qwen3-0.6B-4bit | MLX | 154 tok/s |
| mlx-community/Qwen3-4B-Instruct-2507-4bit | MLX | ran, including Claude Code through `/v1/messages` |
| mlx-community/Qwen3.5-4B-MLX-4bit | MLX | ~52 tok/s; Codex through `/v1/responses` |
| mlx-community/Qwen3.8-27B-4bit | MLX | 11-13 tok/s; Claude Code with tool groups |
| strata/q2_0 (Qwen3.8-Flash-Next, Q2_0) | Strata | 12.9-13.3 tok/s on prose, 9.5-11.5 on code; prompts 14.7 tok/s (134.7 on a 10k prompt with the prompt path on) |
| huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:...-Ternary-Bonsai-PQ2_0 | GGUF, Prism fork | loads and answers correctly; 6.8 tok/s decode, 37 tok/s prompt (519-token request), 7.2 GB |

Speeds and their conditions are in [RESULTS.md](RESULTS.md).
