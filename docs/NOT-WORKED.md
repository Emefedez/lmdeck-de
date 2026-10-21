# Tried, and did not work (or did not help)

Each entry says what was tried, what happened, and what was done instead. The numbers are in
[RESULTS.md](RESULTS.md).

## MLX

- **omarchy-mlx's tuning flags** (`WAVE_SCHED`, `DEFER_COMMIT`, `SDPA_BF16_FAST`): no change in decode speed.
- **`uclampset -m 1024` on the backend** (`server.uclamp_boost`): no difference under the `asahi-performance` tuned
  profile. It may still help on power-saver profiles, where schedutil parks the thread that feeds the GPU.
- **mlx_lm's batched generator**: about 40% slower per stream on this backend. lmdeck serves one stream at a time
  unless `mlx.batching = true`.
- **nvfp4, mxfp4 and mxfp8 models** on omarchy-mlx 0.7.28: there are no GPU kernels for them, so they fail to load.
  Only affine quantization runs. lmdeck probes the installed runtime and marks those models ✗.
- **4-bit quantized KV cache**: wrong results on 0.7.28, and on later releases the output drifts from f16 within a
  few dozen tokens. 8-bit works (see RESULTS).
- **The fused attention kernel at long context**: ~5 GB/s of KV reads. lmdeck uses a composed attention
  (two matmuls and a softmax) for single-token decode instead.
- **mlx-omarchy dev builds of 2026-10-05 to 2026-10-09**: the allocator rounded every GPU buffer over 1 MiB up to a
  power of two, so Qwen3.8-27B-4bit needed 21.4 GiB instead of 15 and stopped fitting. lmdeck briefly built a
  "Fixed" runtime with 32 size classes per power of two. Upstream then shipped 8 per power of two (PR #47, +3% on
  that model), so the Fixed runtime was removed.
- **Loading a 27B model on the stock GPU heap** (50% of RAM): the GPU hung, and a second attempt froze the machine.
  lmdeck now plans every load against the heap, raises it per process (`HK_SYSMEM`) only when needed, and refuses
  loads that do not fit.
- **Coding agents with every MCP tool**: Claude Code sent ~148k tokens of tools (an hour of prefill on the 27B,
  then out of memory), and Codex ~83k. lmdeck drops or groups MCP tools for local models (`server.local_tools`).
- **Benchmark loops with long unique prompts**: 7k-token prompts, each one different, filled RAM with prompt caches
  and checkpoints. Keep such loops short, or lower `mlx.prompt_cache_size`.

## The Strata Vulkan port

- **16-byte weight loads**: an early probe suggested the GEMV needed them. The probe measured the wrong thing. A
  4-byte load is as fast as a 16-byte one, and a 2-byte load runs at a quarter of the speed.
- **Reading misaligned fields as two aligned 4-byte words**: the Q2_0 GEMV fell from 162 to 121 GB/s. A 4-byte load
  at a 2-byte-aligned address is fine.
- **Reading i-quant byte arrays as words** (IQ3_S): the extra arithmetic cost more than the loads saved (92.7 → 84.8
  GB/s).
- **Staging weights through shared memory**: the probe's numbers did not change with buffer size, so they said
  nothing about bandwidth, and the probe was dropped. The weight path is not the GEMV's limit anyway.
- **A matrix-unit GEMV (8x8 cooperative matrices) for speculation windows**: its cost is flat across window sizes,
  as hoped, but it runs at 34 GB/s, so it breaks even only at 8 tokens.
- **Two rows per lane sharing activation loads** (`STRATA_METAL_GEMV_NR=2`): no change to a 2-token window.
- **Wide MTP windows**: `--spec 3` with MTP ran at 8.30 tok/s against 10.49 without drafts. The second token of a
  window costs 60% of a token on the GPU, and rejected drafts still cost their SSD reads. This was measured before
  the SPIR-V cache, which removed part of that cost (kernels compiled on each new window size). What runs now is
  MTP capped at 2 tokens with cost-scheduled verification (`--spec-sched cost`), plus prompt lookup: +21% on code,
  +3% on prose.
- **One command buffer per window, with the GPU spinning on a host flag at each layer**: about 6 us a layer instead
  of 85-115 us, but the GPU was held for 80-300 ms at a time. During a compile the desktop froze, and suspending in
  that state crashed the GPU firmware (a reboot was needed). Each part is now its own submission; the old way is
  opt-in (`STRATA_VK_SPIN=1`).
- **Honeykrisp's compute timestamps**: unusable (they reported 28,000 GB/s). Every time is wall clock.
- **Removing disk encryption for expert streaming**: LUKS measured within noise of the raw partition at
  expert-sized reads, so it stays.
- **Strata's own Vulkan engine upstream**: the issue was closed as not planned (#1214). This port is a fork.
