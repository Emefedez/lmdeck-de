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

8. **Keep the GPU awake through short host waits, bounded.** Splash (`DEVELOPMENT.md`, "Host steps") spins one GPU
   thread only through a host step about as short as the GPU's wake-up, and kapitsa runs a tiny kernel on a second
   queue while a streamed layer waits (`Gpu::keep_warm`: 26-30 -> 43 tok/s on an M5 Pro, the same tokens, ~3x GPU
   power). Strata's old in-command-buffer spin (`STRATA_VK_SPIN=1`) froze the desktop, so this needs a cap on the spin
   and a supervised run with the desktop in use; it targets the 85-115 us per-layer handshake of item 7.
9. **Re-read the conversation while idle (history prefill).** After a reply, kapitsa renders the conversation as the
   client will send it back and reads it before the next turn. Strata's turn checkpoint sits at the previous
   prompt's last `<|im_start|>`, so a follow-up re-reads its own reply (2026-10-24 log: `2983 = 1895 reused + 1088
   read`, the 1088 including a 307-token reply). Pays in human chat; in an agent's tool loop the next request
   arrives before there is idle time.
10. **One set of expert GEMMs for a group of prompt chunks.** Grouping is in (2026-10-25, strata-asahi
   `STRATA_METAL_PROMPT_GROUP`, default 4): a group's chunks run layer by layer and a layer's experts are loaded once,
   storage reads 130 -> 39 GB on a 7.9k prompt with the same answer, but the prompt only 1.5% faster (60.7 vs 61.5 s):
   the reads were hidden behind the GPU. The fixed ~6-7 s a chunk is GPU work: each chunk's own expert GEMMs decode
   every routed expert's weights for ~40 rows an expert. One GEMM over the group's rows (as Splash runs it) amortizes
   that over ~160 rows; XS/HS/YE sized for the group cost ~0.5 GB a chunk. Estimated up to a third off a long prompt;
   unmeasured.
11. **Session files on the SSD automatically**, not only around unloads: Splash and kapitsa write a conversation's
   restore point ~10 s after a turn when idle (2048+ tokens deep), CRC-checked, and take them back after a restart.
   lmdeck now saves/restores around its own unloads (`strata.sessions`); per-turn writes would also cover a crash
   and several conversations.
12. **Already done in Strata, for the record**: a prompt's streamed experts enter the arena's LRU tail, so decode-hot
   experts survive a long prompt (`stream.cpp`, the scan-victim rule kapitsa added in its store); and balanced prompt
   chunks (2026-10-25, `STRATA_METAL_PROMPT_BAL`, after kapitsa's `NI_BAL`).

Splash (incoai/splash, Apache-2.0) serves the same Flash-Next Q2_0 GGUF and reports, on an M5 Pro 64 GB against
Strata on that Mac: decode 95 tok/s (1.3x), 32K prefill 1,312 tok/s (1.3x), and a cached 32K replay's first token
in 171 ms (212x). The last number is the prefix cache, not the kernels: it is what items 9-11 and lmdeck's
`strata.prefix_reuse` / `strata.sessions` go after. LM Studio ships Splash as a downloadable runtime ("Splash
(Metal)", experimental backends); its integration is LM Studio's closed code, Splash's own server is in the repo.

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
4. **Engine status in the Home screen.** Splash's `/status` reports the streamed experts' hit rate, bytes read and
   host-step round trips per use (prefill / decode), and kapitsa logs how many of a request's experts were in memory.
   Strata's `/v1/status` and engine log carry similar numbers; showing the hit rate beside the context meter would
   tell at a glance why a turn was slow.
5. **Release the weights, keep the caches.** Splash's idle release (`--idle-release`, `POST /v1/memory/release`)
   frees the weights' memory but keeps the prefix cache, and restores the weights on the next request. lmdeck's
   keep_alive kills the backend; for Strata a release that keeps the conversation cache would make the 15-17 s
   reload the only cost of an idle unload.
6. **A llama.cpp baseline for Flash-Next.** kapitsa measured llama.cpp streaming the Q2_0 GGUF by mmap at ~39 tok/s
   on an M5 Pro 48 GB (kapitsa 40). On Asahi llama.cpp's Vulkan backend would need the experts on the CPU side
   (`-ot exps=CPU`); one measured run would say whether Strata's port is ahead. Not run (needs a go-ahead: memory).

