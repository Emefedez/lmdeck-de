**Quantized KV cache (`kv_bits`) fails: QuantizedMatmul refuses the 5-D broadcast weights mlx-lm's quantized SDPA passes**

`mlx_lm.models.base.quantized_scaled_dot_product_attention` (mlx-lm 0.31.3) expands keys/values to `[B, n_kv, 1, S, D·bits/32]` and queries to `[B, n_kv, n_repeats, L, D]` for GQA; `ops.cpp` then broadcasts the weights across the repeat axis, and the omarchy QuantizedMatmul refuses anything above 3-D:

```
[omarchy] QuantizedMatmul weight layout is not implemented for the Omarchy Vulkan backend (dtype=bfloat16, shape=[1,2,4,1,4])
```

so `--kv-bits` / `QuantizedKVCache` can't be used on omarchy-mlx at all.

Every group of `n_repeats · L` query rows uses the same keys, so the repeat can be folded into the row count instead of broadcasting (no copy of the cache per repeat): reshape queries to `[B·n_kv, n_repeats·L, D]` and the quantized keys/values to `[B·n_kv, S, ·]`, run the two `quantized_matmul`s, and reshape the scores back to 5-D only for the mask. Checked against `mx.fast.scaled_dot_product_attention` on the dequantized cache: max |diff| ≤ 0.006 for 8-bit and ≤ 0.008 for 4-bit (bf16) on v0.7.31. On the 0.7.28 package the 4-bit result is wrong (max |diff| 0.4-0.85), which looks like the batched qmm stride bug fixed in 6c4b9c48.

Measured with Qwen3.8-27B-4bit (M1 Max) at 12k context: 8-bit KV shrinks the cache from 0.93 to 0.56 GiB with identical greedy output (64/64) at ~10-15% lower decode speed; 4-bit shrinks it to 0.37 GiB but greedy output diverges within 14-29 tokens. This could ship as one more mlx-lm patch in the series (or as ND batch support in QuantizedMatmul when the extra dims are contiguous or broadcast).
