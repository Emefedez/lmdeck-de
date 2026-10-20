**bf16 decode SDPA streams ~5 GB/s of KV at long context; a composed matmul route is 2-3.6x faster**

Single-query (L = 1) `mx.fast.scaled_dot_product_attention` in bf16 gets slow as the context grows. On an M1 Max (Honeykrisp, v0.7.31 and the 0.7.28 package alike), Qwen3.8-27B-shaped attention (24 query heads, 4 KV heads, head_dim 256), KV a slice of a larger cache buffer:

| S | fused SDPA | composed (q@kᵀ, f32 softmax, p@v, GQA folded into rows) | fused effective KV bandwidth |
|---|---|---|---|
| 1024 | 0.94 ms | 0.48 ms | 4.1 GB/s |
| 4096 | 2.33 ms | 0.96 ms | 6.7 GB/s |
| 12288 | 8.42 ms | 2.34 ms | 5.6 GB/s |
| 24576 | 16.56 ms | 4.81 ms | 5.7 GB/s |

Other GQA shapes (16/2/256, 40/8/128, 32/8/128) show the same at S ≥ 4096 (1.1-2.4x); at S = 512 with head_dim 128 the fused kernel is still faster. Prompt chunks (L = 512, causal) are fine: fused and composed are within a few percent.

End to end, Qwen3.8-27B-4bit decode drops from 12.0 tok/s at 1k context to 4.8 tok/s at 12k with the fused kernel; routing L == 1, S ≥ 1024 through the composed form gives 8.45 tok/s at 12k (+76%) and 11.3 vs 9.3 tok/s at 4k. Teacher-forced over 64 decode steps at 1.5k context: top-1 63/64, median KL(fused‖composed) 1.3e-4, max 3.3e-3.

From the comment above `SDPA_DECODE_NATIVE` in primitives.cpp, the bf16 decode arm uses the one-pass 32-stream merge at every context length, so a long context runs on few workgroups. A two-pass/split-K arm for bf16, or routing long-context bf16 decode to the composed matmuls, would close most of the gap. Repro script attached (runs in ~1 min, no model needed).
