# Models

Which models run depends on the backend, its version and your free memory, so lmdeck checks rather than relying on
a fixed list. This page says how it checks, and which models have actually been run here.

## How lmdeck tells you before you download

- **Get models** shows each variant's estimated memory (weights plus the attention cache for your context settings
  plus runtime overhead) and whether it fits now: fits, tight (needs a raised GPU heap and the free memory for it),
  or too big.
- **MLX compatibility markers**: ✓ qualified by the omarchy-mlx project on this backend (its catalog, shown under
  Get models), ? untested, ✗ will not run. lmdeck probes which quantization modes the installed runtime can run on
  the GPU, and gives a ✗ with a recommended alternative where one is known.
- **Models** grades what you have downloaded (see the README's grader section).

## By backend

| Backend | Runs | Does not run |
|---|---|---|
| MLX (omarchy-mlx 0.7.28 package) | MLX safetensors with affine quantization (4/8-bit `mlx-community` repos), dense or hybrid (Qwen3.5/3.6/3.8 attention + linear layers), mixture of experts | nvfp4, mxfp4 and mxfp8 quantization (no GPU kernels); models with ops the backend lacks (lmdeck offers the CPU, much slower) |
| GGUF (llama.cpp, Vulkan) | What llama.cpp supports. Partial GPU offload when a model does not fit whole (`llama.auto_offload`) | Not benchmarked here |
| Strata (strata-asahi) | Qwen3.8-Flash-Next packs set up by Strata's setup (`strata-<model>.json`) | Any other model: Strata runs this one model family |

The most memory a model can have depends on what else runs. On this 32 GB machine, with the desktop running,
lmdeck admits MLX models up to about 19 GiB of weights, for example a 35B-A3B at 4-bit. It needs about 23 GiB free
to do so (use **Make room** first).

## Run on this machine

| Model | Backend | Result |
|---|---|---|
| mlx-community/Qwen3-0.6B-4bit | MLX | 154 tok/s |
| mlx-community/Qwen3-4B-Instruct-2507-4bit | MLX | ran, including Claude Code through `/v1/messages` |
| mlx-community/Qwen3.5-4B-MLX-4bit | MLX | ~52 tok/s; Codex through `/v1/responses` |
| mlx-community/Qwen3.8-27B-4bit | MLX | 11-13 tok/s; Claude Code with tool groups |
| strata/q2_0 (Qwen3.8-Flash-Next, Q2_0) | Strata | 12.9-13.3 tok/s on prose, 9.5-11.5 on code |

Speeds and their conditions are in [RESULTS.md](RESULTS.md).
