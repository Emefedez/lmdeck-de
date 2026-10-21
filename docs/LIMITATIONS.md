# Limitations

## Tested on one machine

A MacBook Pro M1 Max with 32 GB, Asahi Linux 7.1.12, Omarchy. Other Apple chips, other RAM sizes and other
distributions are untested. Memory planning assumes Honeykrisp's rules (a stock GPU heap of 50% of RAM, raised per
process with `HK_SYSMEM`), and Strata's arena budget assumes 32 GB or more.

## GPU memory can take the machine down

Overcommitting the Apple GPU's memory under Asahi can hang the GPU and freeze the desktop, and only a forced power-off
recovers it. lmdeck plans every load and refuses ones that do not fit (`server.gpu_mem_check`; turning it off is not
safe). Its estimates are conservative but not guarantees, especially for models with unusual architectures.

## Strata

- Prompts are read at ~10 tok/s until the prompt kernels are wired in. A 2,000-token prompt takes over 3 minutes.
- Only the Q2_0 pack has been run on Asahi. Vision input is untested.
- **Do not suspend the machine during a Strata run.** Suspending while the engine held the GPU crashed the GPU
  firmware, and a reboot was needed. The default mode now frees the GPU between parts of a window, but suspend has
  not been tested again.
- Before loading, close large GPU users. Strata needs ~9 GiB for its dense weights plus an expert arena; the arena
  shrinks to fit free memory, and decode slows as it shrinks (4 GB arena: 8.6 tok/s; 8 GB: 10.5 tok/s, no drafts).
- It uses a private Mesa build (honeykrisp-strata v3, an unmerged branch). It can lag behind system Mesa fixes.

## MLX (omarchy-mlx)

- Only affine-quantized models run on the GPU with the 0.7.28 package (no nvfp4/mxfp4/mxfp8 kernels). Models with
  ops the backend lacks fail to load. The CPU fallback works but is much slower.
- 4-bit KV quantization is wrong or drifts. 8-bit works, ~10-15% slower.
- Load progress is estimated from GPU shared-memory growth, because Asahi has no per-process GPU memory stats.

## lmdeck

- One model is resident by default (`server.max_loaded = 1`). A second large model on unified memory risks swapping.
- The context guard estimates prompt size at ~3.8 characters per token, so it can be off by ~10% either way.
- Coding agents on local models get trimmed tools and prompts (`server.local_tools`, `server.local_prompt`). That is
  what makes them usable, but they are not the agent's full setup.
- The grader's intelligence column is a size estimate unless you give it scores. Speed and power are recorded only with `usage.perf = true` (off by
  default); otherwise speed is an estimate and power is unknown. Power needs the SMC's "Total System Power" sensor (Apple laptops under Asahi) and counts the
  whole machine, display included.
- The bar panel replaces Omarchy's stock Agents widget by forking its `Panel.qml`. After an Omarchy update, re-run
  `omarchy-plugin/m1-max.agents/sync-from-stock.py`.
- GGUF (llama.cpp) works but has not been benchmarked here.
