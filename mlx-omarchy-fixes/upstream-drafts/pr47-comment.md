Data point from a dense model, in case finer classes are worth it: Qwen3.8-27B-4bit (2,180 tensors, 14.95 GiB in safetensors, M1 Max 32 GB) needs this much device memory for its weights under each rule:

| rule | weights on device | over exact |
|---|---|---|
| power of two (b20211a0, v0.7.31) | 21.41 GiB | +6.45 GiB (+43%) |
| 8 per octave (this PR) | 15.45 GiB | +0.50 GiB (+3.3%) |
| 32 per octave | 15.07 GiB | +0.11 GiB (+0.8%) |

Computed from the safetensors headers with the same rounding as `round_size`. On a 32 GB machine the Honeykrisp heap headroom for this model is a couple of GiB once the KV cache and prompt-cache checkpoints are counted, so the extra 0.4 GiB at 8 per octave is noticeable for 19-20 GiB models (e.g. an OptiQ mixed 4/8-bit 27B at 19.2 GiB). I've been running 32 per octave locally on v0.7.31 (same formula, `octave / 32` step, page-aligned): a 70 MiB buffer takes 70 MiB, a 300 MiB one 304 MiB, and the growing-KV reuse still lands in shared classes. Not a blocker for this PR, just a possible follow-up.
