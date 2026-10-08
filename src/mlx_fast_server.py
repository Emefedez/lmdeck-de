"""lmdeck launcher for mlx_lm.server with a decode fast path.

mlx_lm.server (0.31.x) calls `logprobs[token].item()` for every generated
token, even when the client did not ask for logprobs. Each call is an extra
synchronous GPU round trip that defeats stream_generate's async pipelining;
on omarchy-mlx (Vulkan) it halves decode throughput. This shim loads the
server module from source, makes that read conditional on the request's
`logprobs` flag, and runs it. It also routes requests through the
single-stream path (stream_generate) unless LMDECK_MLX_BATCH=1, since the
batched generator is slower per stream here. Unknown versions run unpatched.
"""

import os
import sys
import types

import mlx_lm.server as _orig

PATCHES = [
    # single-request path
    (
        "gen.logprobs[gen.token].item(),",
        "(gen.logprobs[gen.token].item() if args.logprobs else 0.0),",
    ),
    # batched path: remember the flag per request ...
    (
        '"top_logprobs": args.top_logprobs,',
        '"top_logprobs": args.top_logprobs, "logprobs": args.logprobs,',
    ),
    # ... and only sync when it was requested
    (
        "r.logprobs[r.token].item(),",
        '(r.logprobs[r.token].item() if result.get("logprobs") else 0.0),',
    ),
]


def enable_cpu_mode():
    """Run the whole model on the CPU (opt-in, discouraged: far slower).

    omarchy-mlx refuses silent CPU fallback and its CPU backend lacks the
    mx.fast kernels models call (rms_norm, layer_norm, rope, attention), so
    swap those for plain-op equivalents that do exist on the CPU."""
    import mlx.core as mx

    mx.set_default_device(mx.cpu)
    f32 = mx.float32

    def rms_norm(x, weight, eps):
        xf = x.astype(f32)
        y = xf * mx.rsqrt(mx.mean(xf * xf, axis=-1, keepdims=True) + eps)
        if weight is not None:
            y = y * weight.astype(f32)
        return y.astype(x.dtype)

    def layer_norm(x, weight, bias, eps):
        xf = x.astype(f32)
        mu = mx.mean(xf, axis=-1, keepdims=True)
        var = mx.var(xf, axis=-1, keepdims=True)
        y = (xf - mu) * mx.rsqrt(var + eps)
        if weight is not None:
            y = y * weight.astype(f32)
        if bias is not None:
            y = y + bias.astype(f32)
        return y.astype(x.dtype)

    def rope(a, dims, *, traditional, base, scale, offset, freqs=None):
        L = a.shape[-2]
        steps = mx.arange(L, dtype=f32)
        off = offset if isinstance(offset, mx.array) else mx.array(offset)
        off = off.astype(f32)
        pos = (off.reshape(-1, 1, 1) + steps.reshape(1, L, 1)) if off.ndim > 0 else (off + steps).reshape(L, 1)
        pos = pos * scale
        inv = (1.0 / freqs.astype(f32)) if freqs is not None else base ** (-mx.arange(0, dims, 2, dtype=f32) / dims)
        theta = pos * inv  # (L, d/2) or (B, L, d/2)
        if theta.ndim == 3:
            theta = theta[:, None]  # broadcast over heads
        cos, sin = mx.cos(theta), mx.sin(theta)
        x = a[..., :dims].astype(f32)
        if traditional:
            x1, x2 = x[..., 0::2], x[..., 1::2]
            out = mx.stack([x1 * cos - x2 * sin, x1 * sin + x2 * cos], axis=-1).reshape(x.shape)
        else:
            h = dims // 2
            x1, x2 = x[..., :h], x[..., h:]
            out = mx.concatenate([x1 * cos - x2 * sin, x1 * sin + x2 * cos], axis=-1)
        out = out.astype(a.dtype)
        return mx.concatenate([out, a[..., dims:]], axis=-1) if dims < a.shape[-1] else out

    def scaled_dot_product_attention(q, k, v, *, scale, mask=None, sinks=None, **_):
        hq, hk = q.shape[1], k.shape[1]
        if hq != hk:
            k = mx.repeat(k, hq // hk, axis=1)
            v = mx.repeat(v, hq // hk, axis=1)
        s = (q.astype(f32) * scale) @ k.astype(f32).swapaxes(-1, -2)
        L, S = q.shape[-2], k.shape[-2]
        if isinstance(mask, str):
            if mask == "causal":
                qi = mx.arange(S - L, S).reshape(L, 1)
                ki = mx.arange(S).reshape(1, S)
                s = mx.where(qi >= ki, s, mx.array(-float("inf"), f32))
        elif mask is not None:
            s = mx.where(mask, s, mx.array(-float("inf"), f32)) if mask.dtype == mx.bool_ else s + mask.astype(f32)
        if sinks is not None:
            sink = mx.broadcast_to(sinks.astype(f32).reshape(1, -1, 1, 1), (*s.shape[:-1], 1))
            p = mx.softmax(mx.concatenate([sink, s], axis=-1), axis=-1)[..., 1:]
        else:
            p = mx.softmax(s, axis=-1)
        return (p @ v.astype(f32)).astype(q.dtype)

    mx.fast.rms_norm = rms_norm
    mx.fast.layer_norm = layer_norm
    mx.fast.rope = rope
    mx.fast.scaled_dot_product_attention = scaled_dot_product_attention
    print("lmdeck: running on the CPU (discouraged: much slower than the Apple GPU)", file=sys.stderr, flush=True)


def kv_quant_probe(bits, group):
    """True if quantized KV attention runs on this runtime, else the error text."""
    try:
        import mlx.core as mx
        from mlx_lm.models.base import quantized_scaled_dot_product_attention
        from mlx_lm.models.cache import QuantizedKVCache

        cache = QuantizedKVCache(group_size=group, bits=bits)
        k = mx.random.normal((1, 2, 4, 64)).astype(mx.bfloat16)
        qk, qv = cache.update_and_fetch(k, k)
        q = mx.random.normal((1, 8, 1, 64)).astype(mx.bfloat16)
        out = quantized_scaled_dot_product_attention(q, qk, qv, scale=0.125, mask=None, group_size=group, bits=bits)
        mx.eval(out)
        return True
    except Exception as e:  # noqa: BLE001 - any failure means "not supported here"
        return str(e).splitlines()[0][:200]


def install_kv_options(mod):
    """KV cache options mlx_lm.server does not expose: quantized KV and a
    rotating window. Set through LMDECK_MLX_* env vars by lmdeck."""
    bits = int(os.environ.get("LMDECK_MLX_KV_BITS", "0") or 0)
    group = int(os.environ.get("LMDECK_MLX_KV_GROUP", "64") or 64)
    start = int(os.environ.get("LMDECK_MLX_KV_START", "0") or 0)
    max_kv = int(os.environ.get("LMDECK_MLX_MAX_KV", "0") or 0)
    notes = []
    if bits:
        ok = kv_quant_probe(bits, group)
        if ok is True:
            notes.append(f"KV {bits}-bit (group {group}, from token {start})")
        else:
            print(f"lmdeck: {bits}-bit KV not supported by this omarchy-mlx runtime ({ok}); using f16 KV", file=sys.stderr, flush=True)
            bits = 0
    if max_kv:
        notes.append(f"KV window {max_kv} tokens")
    if not bits and not max_kv:
        return "KV f16"

    orig_stream = mod.stream_generate

    def stream_generate(*args, **kwargs):
        if bits:
            kwargs.setdefault("kv_bits", bits)
            kwargs.setdefault("kv_group_size", group)
            kwargs.setdefault("quantized_kv_start", start)
        return orig_stream(*args, **kwargs)

    mod.stream_generate = stream_generate
    if max_kv and hasattr(mod, "make_prompt_cache"):
        orig_cache = mod.make_prompt_cache

        def make_prompt_cache(model, max_kv_size=None):
            return orig_cache(model, max_kv_size=max_kv_size or max_kv)

        mod.make_prompt_cache = make_prompt_cache
    return ", ".join(notes)


# Optional: checkpoint the conversation prefix before generating. The stock single-request
# path only caches prompt+output, which a hybrid model (non-trimmable cache, e.g. Qwen3.5/3.8)
# can never reuse, so every agent turn re-read the whole prompt.
OPTIONAL_PATCHES = [
    (
        "            # Process the prompt and generate tokens\n            for gen in stream_generate(",
        "            if self.model_provider.draft_model is None:\n"
        "                rest = _lmdeck_checkpoint(self, model, tokenizer, prompt, rest, cache, progress)\n"
        "            # Process the prompt and generate tokens\n            for gen in stream_generate(",
    ),
]


def lmdeck_checkpoint(server, model, tokenizer, prompt, rest, cache, progress):
    """Prefill up to the last <|im_start|> (the start of the answer being generated), save
    that cache as a prefix entry, and return the tokens still to process."""
    import copy

    import mlx.core as mx

    try:
        im = tokenizer.convert_tokens_to_ids("<|im_start|>")
    except Exception:
        return rest
    if not isinstance(im, int) or im < 0:
        return rest
    start = len(prompt) - len(rest)
    try:
        boundary = len(prompt) - 1 - prompt[::-1].index(im)
    except ValueError:
        return rest
    if boundary - start < 256:
        return rest
    step = getattr(server.cli_args, "prefill_step_size", None) or 512
    toks = prompt[start:boundary]
    for i in range(0, len(toks), step):
        chunk = toks[i : i + step]
        model(mx.array(chunk)[None], cache=cache)
        mx.eval([c.state for c in cache])
        progress(start + i + len(chunk), len(prompt))
    server.prompt_cache.insert_cache(
        server.model_provider.model_key, prompt[:boundary], copy.deepcopy(cache), cache_type="user"
    )
    return prompt[boundary:]


def main():
    if os.environ.get("LMDECK_MLX_DEVICE") == "cpu":
        enable_cpu_mode()
    path = _orig.__file__
    src = open(path, encoding="utf-8").read()
    applied = 0
    for old, new in PATCHES:
        if src.count(old) == 1:
            src = src.replace(old, new)
            applied += 1
    checkpoint = False
    for old, new in OPTIONAL_PATCHES:
        if applied == len(PATCHES) and src.count(old) == 1:
            src = src.replace(old, new)
            checkpoint = True
    if applied != len(PATCHES):
        print(
            f"lmdeck: mlx_lm.server fast path not applicable ({applied}/{len(PATCHES)} patches matched); running stock server",
            file=sys.stderr,
            flush=True,
        )
        return _orig.main()
    mod = types.ModuleType("mlx_lm.server")
    mod.__file__ = path
    mod.__package__ = "mlx_lm"
    sys.modules["mlx_lm.server"] = mod
    mod.__dict__["_lmdeck_checkpoint"] = lmdeck_checkpoint
    exec(compile(src, path, "exec"), mod.__dict__)
    # The BatchGenerator path is ~40% slower per stream than stream_generate on
    # this backend; a desktop rarely runs concurrent requests, so default to the
    # single-request path unless batching is asked for.
    batching = os.environ.get("LMDECK_MLX_BATCH") == "1"
    rg = getattr(mod, "ResponseGenerator", None)
    if not batching and rg is not None and hasattr(rg, "_is_batchable"):
        rg._is_batchable = lambda self, args: False
    kv = install_kv_options(mod)
    print(
        f"lmdeck: mlx_lm.server fast path active (no per-token logprob sync, batching {'on' if batching else 'off'}, {kv}"
        f"{', prefix checkpoints' if checkpoint else ''})",
        file=sys.stderr,
        flush=True,
    )
    return mod.main()


if __name__ == "__main__":
    main()
