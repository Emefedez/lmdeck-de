# Ideas

These are untested, so each one comes with the reason to expect it to help.

## Strata on Vulkan

1. **Push the prompt path past 10x.** It is measured now: 134.7 tok/s on a 10k-token prompt against 14.7 through
   decode windows (9.2x, identical answer; RESULTS.md). What is left for lmdeck's 10x target is a cheaper per-chunk
   pass over the arena's missed experts or an 8192-row chunk (about 8.9 GB of arena, so less decode cache), plus the
   desktop-sharing check. The path is off by default until then.
2. **Overlap weight loads with arithmetic in the GEMV.** The dense GEMV's time is the sum of its loads and its
   math (163 GB/s, with the weight path alone at 322). Fetching the next unit before decoding the current one
   should move it towards ~230 GB/s.
3. **A matrix-unit GEMV that is fast at 1-4 tokens.** Speculation stops paying when a second token costs 60% of a
   token. The 8x8 cooperative-matrix GEMV's cost is flat across window sizes but slow (34 GB/s). Decoding the
   weights once per tile instead of once per simdgroup could fix that.
4. **Skip the SSD reads of rejected draft rows.** A rejected draft's experts are read anyway (+14-37 ms of host
   turns at 2 tokens).
5. **Hyper-connection reads** (`gr_down_q8`, 1.6 → 3.5 ms a class for the second token). It is the same
   activation-traffic problem as 3.
6. **Other packs on Asahi.** IQ3_XXS, IQ3_S and the Unsloth UD-Q4..Q6 packs run on Macs upstream. At 32 GB they
   would stream more experts than Q2_0. Untested here.
7. **Per-layer handshake cost.** Per-part submits cost 85-115 us a layer (~5 ms a window). A GPU-side wait that
   yields the GPU (instead of spinning) would get the 6 us path without holding the GPU.

## MLX

1. **A split-K bf16 decode attention kernel upstream.** The composed route lmdeck uses is 2-3.6x faster than the
   fused kernel at long context. Fixing the kernel helps every omarchy-mlx user. A report with
   `bench/sdpa_decode.py` was drafted, not filed.
2. **Batched quantized matmul for KV quantization upstream.** lmdeck folds the GQA repeat into the query rows to make
   `kv_bits` work. The same fix belongs in mlx-lm's quantized SDPA or in QuantizedMatmul.
3. **Attention layers at decode**: 49-87 GB/s against 183 GB/s for the MLP layers on the 27B. That is the next
   place to look after the kernel above.

## lmdeck

1. **A power table.** With `usage.perf = true`, lmdeck records system watts while generating. After some use, `lmdeck grade efficient`
   could print J/token per model as a measured table for RESULTS.md.
2. **GGUF benchmarks.** llama.cpp on Honeykrisp has never been measured here against the same model in MLX.
3. **Grader scores from model cards.** Some Hugging Face cards carry evaluation results (`model-index`). Reading
   them when a model is downloaded, not on every grade, could replace the size estimate where they exist.
