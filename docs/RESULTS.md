# Results

Everything here was measured on a MacBook Pro M1 Max with 32 GB, Asahi Linux 7.1.12 and Omarchy, between 2026-10-08
and 2026-10-21. Decoding is greedy unless a row says otherwise. Where a number has its own commands and raw output,
the section links to them.

## MLX (omarchy-mlx)

### Decode speed and lmdeck's fast path

Stock `mlx_lm.server` 0.31 calls `logprobs[token].item()` on every token, a synchronous GPU round trip, whether or
not the client asked for logprobs. lmdeck's launcher (`src/mlx_fast_server.py`) skips it unless logprobs were
requested.

| Qwen3-0.6B-4bit, 256 tokens, legacy omarchy-mlx wheel | decode tok/s |
|---|---|
| stock `mlx_lm.server` | 34-35 |
| lmdeck fast path | 62-81 |
| `mlx_lm.generate` (no server) | 62-77 |

Moving from the legacy wheel to the omarchy-mlx 0.7.28 packages took the same model from 67 to 154 tok/s.

| Model (package runtime 0.7.28 unless noted) | Decode | Prompt | Load |
|---|---|---|---|
| Qwen3-0.6B-4bit | 154 tok/s | | |
| Qwen3.5-4B-MLX-4bit | ~52 tok/s short, 33 tok/s at 7k context | ~467 tok/s | |
| Qwen3.8-27B-4bit | 11-13 tok/s | 60-70 tok/s | ~9 s |

Qwen3.5-4B ran at the same speed on the 0.7.28 package and on the v0.7.33 release.

### Decode attention at long context

omarchy-mlx's fused attention kernel streams only ~5 GB/s of KV cache when one new token attends a long context.
Shapes are Qwen3.8-27B's (24 query heads, 4 KV heads, head_dim 256), with the KV cache a slice of a larger buffer.
The benchmark is `bench/sdpa_decode.py`.

| Context | Fused SDPA | Composed (q·kᵀ, f32 softmax, p·v, GQA folded into rows) | Fused, effective KV GB/s |
|---|---|---|---|
| 1024 | 0.94 ms | 0.48 ms | 4.1 |
| 4096 | 2.33 ms | 0.96 ms | 6.7 |
| 12288 | 8.42 ms | 2.34 ms | 5.6 |
| 24576 | 16.56 ms | 4.81 ms | 5.7 |

End to end on Qwen3.8-27B-4bit, routing single-token decode over 1k+ tokens through the composed form
(`mlx.decode_attention = composed`, the default) gave 9.3 → 11.3 tok/s at 4k context and 4.8 → 8.5 tok/s at 12k.
Teacher-forced against the fused kernel over 64 steps at 1.5k context: top-1 63/64, median KL 1.3e-4, max 3.3e-3.

### Quantized KV cache

Qwen3.8-27B-4bit at 12k tokens:

| `mlx.kv_bits` | KV cache | Output | Decode |
|---|---|---|---|
| 0 (f16) | 0.93 GiB | reference | reference |
| 8 | 0.56 GiB | identical greedy output (64/64) | 10-15% slower |
| 4 | 0.37 GiB | diverges within 14-29 tokens | |

### GPU buffer rounding

The weights of Qwen3.8-27B-4bit (2,180 tensors, 14.95 GiB in safetensors) in device memory, under each allocator
rule:

| Rule | Weights on the GPU |
|---|---|
| power of two (mlx-omarchy dev builds 2026-10-05 to 2026-10-09) | 21.41 GiB (+43%): no longer fits |
| 8 classes per power of two (upstream since PR #47, v0.7.32+) | 15.45 GiB (+3.3%) |
| 32 classes per power of two (tried locally) | 15.07 GiB (+0.8%) |

lmdeck's planner models each runtime's rule, so it refuses a load that would not fit before the load starts.

### Coding agents

| What | Before | After |
|---|---|---|
| Claude Code's request with 9 MCP servers (`server.local_tools`) | ~148k tokens: ~1 h of prefill on the 27B, then out of memory | `lean` ~6.6k tokens, first answer in ~100 s on Qwen3.8-27B |
| Claude Code's system prompt (`server.local_prompt = lean`) | ~8.1k tokens | ~4.5k tokens; a new conversation in a new project 104 s → 25 s (27B) |
| Follow-up turn on a hybrid model (prefix checkpoints) | ~99 s | ~13 s |
| New Claude Code conversation after a restart (system-prompt checkpoint on disk) | ~104 s | ~50 s |
| Codex request (`/v1/responses`, MCP servers as namespaces) | 284 tools, ~83k tokens | ~8k tokens; on Qwen3.5-4B: first turn 23 s, follow-up 3 s |

A Codex run on Qwen3.5-4B ended at 3.45 GiB of MLX memory in use, 0.83 GiB cached and a 3.71 GiB peak, with
`mlx.cache_limit_gib = 1`.

## The driver (honeykrisp-strata)

Stock Mesa 26.2.4 against honeykrisp-strata's v3 build (`f144c7949b`). The commands are in honeykrisp-strata's
[README](https://github.com/Emefedez/honeykrisp-strata) and
[bench/RESULTS.md](https://github.com/Emefedez/honeykrisp-strata/blob/main/bench/RESULTS.md).

| | stock 26.2.4 | v3 |
|---|---|---|
| GPU heap | 15.45 GiB (50% of RAM) | 27.81 GiB (90%) |
| bf16 cooperative matrices | no | yes |
| Streaming read (uvec4) | ~320 GB/s | ~320 GB/s |
| Dependent tiny dispatch | 3.5 us | 2.5 us |
| Per-layer handshake: submit + fence | 115 us | 85 us |
| Per-layer handshake: GPU spins on a host flag | 6.0 us | 5.8 us |

Load width on v3, 512 MiB: 16-byte loads 288-300 GB/s, 8-byte 274-301 GB/s, 4-byte 260-282 GB/s, 2-byte 67-72 GB/s.
A 4-byte load may sit at any 2-byte-aligned address at full speed.

## SSD (expert streaming)

Random O_DIRECT reads, 6-12 threads, 3 GiB a run:

| Chunk | Raw partition | dm-crypt (LUKS) | File on btrfs (zstd, on LUKS) |
|---|---|---|---|
| 1792 KiB (one expert) | 7.15 GB/s | 7.17 GB/s | 6.80-6.94 GB/s |
| 592 KiB (one matrix) | 6.49 GB/s | 6.11 GB/s | 5.39-5.66 GB/s |

Encryption costs nothing measurable at expert-sized reads. Btrfs costs 4-7%, so Strata's setup marks the model
folder `chattr +C`. Upstream measured 2.97 GB/s on a 32 GB M4 Mac mini under macOS.

## Strata (Qwen3.8-Flash-Next, Vulkan)

Q2_0, experts streamed from the SSD, 8 GB expert arena, warm SPIR-V cache, 150 tokens generated. The full tables
and commands are in strata-asahi's
[vulkan/RESULTS.md](https://github.com/Emefedez/strata-asahi/blob/vulkan-asahi/vulkan/RESULTS.md).

| Prompt | No drafts | Prompt lookup (`--spec 8 --suffix-draft 3`) | + MTP, cost-scheduled (`--mtp ... --mtp-max-t 2 --spec-sched cost`) |
|---|---|---|---|
| Story, 28 tokens | 12.94 tok/s | 12.59 tok/s | 13.34 tok/s (1.32 tokens a round) |
| Code rewrite, 132 tokens | 9.50 tok/s | 11.19 tok/s | 11.54 tok/s (3.43 tokens a round) |

- Expert arena size, story prompt, no MTP: 4 GB → 8.56 tok/s (hit rate 0.71, 283 MB a token from the SSD); 8 GB →
  10.49 tok/s (0.84, 201 MB); polling the handshake fence every 4096 spins instead of every spin → 11.63 tok/s.
- Prompts are read through decode windows at ~10 tok/s (7.3 before the SPIR-V cache). The code prompt's time to
  first token went 22.9 → 15.8 s.
- Loading through lmdeck: 13.9 s with a 13.8 GB arena. Free memory stayed above 7.96 GB.
- Where a 1-token window's time goes: 39.5 ms of GPU between the 48 per-layer handshakes, 22-25 ms of host turns
  (SSD reads), 3.1 ms tail. Of the GPU time, the experts take 11 ms, the hyper-connection reads ~8 ms and the LM head
  2.8 ms.
- Prompt kernels (not wired into the engine yet): the dense GEMM reaches 4.2 TFLOP/s; the expert GEMM sets a
  ceiling of 274 tok/s for prompt reading.
- The dense GEMV (Q2_0, 16384 x 16384, one token) reaches 163 GB/s, while its weight reads alone reach 322 GB/s.

Upstream's number for a 32 GB Mac under macOS is 7.9 tok/s decode and 186 tok/s prompt (docs/MAC.md), measured with
different prompts.

## What was not measured

- GGUF through llama.cpp has no benchmark in this project.
- Power draw per model: lmdeck can record it (`usage.perf = true`, off by default), but no table exists yet.
- Any machine other than this M1 Max.
