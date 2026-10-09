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


def folded_quantized_sdpa(queries, q_keys, q_values, scale, mask, group_size=64, bits=8):
    """Quantized SDPA with the GQA repeat folded into the query rows.

    mlx_lm expands keys/values to 5-D and lets quantized_matmul broadcast them over
    the repeat axis; omarchy-mlx's kernel only takes 2-D/3-D weights (and a
    materialized broadcast would copy the cache once per repeat). Every group of
    n_repeats*L query rows uses the same keys, so fold them into the row count.
    """
    import mlx.core as mx

    B, n_q_heads, L, D = queries.shape
    n_kv_heads = q_keys[0].shape[-3]
    S = q_keys[0].shape[-2]
    R = n_q_heads // n_kv_heads
    q = (queries * scale).reshape(B * n_kv_heads, R * L, D)
    k = tuple(t.reshape(B * n_kv_heads, S, t.shape[-1]) for t in q_keys)
    v = tuple(t.reshape(B * n_kv_heads, S, t.shape[-1]) for t in q_values)
    scores = mx.quantized_matmul(q, *k, transpose=True, group_size=group_size, bits=bits)
    scores = scores.reshape(B, n_kv_heads, R, L, S)
    if mask is not None:
        if isinstance(mask, str):
            mask = mx.arange(S - L, S)[:, None] >= mx.arange(S)[None]
        if mask.ndim == 4:  # [B|1, H|1, L, S]: broadcast over the repeat axis
            mask = mx.expand_dims(mask, 2) if mask.shape[1] == 1 else mask.reshape(mask.shape[0], n_kv_heads, R, L, S)
        if mask.dtype == mx.bool_:
            scores = mx.where(mask, scores, mx.finfo(scores.dtype).min)
        else:
            scores = scores + mask
    scores = mx.softmax(scores, axis=-1, precise=True)
    out = mx.quantized_matmul(
        scores.reshape(B * n_kv_heads, R * L, S), *v, transpose=False, group_size=group_size, bits=bits
    )
    return out.reshape(B, n_q_heads, L, D)


# Decode attention: omarchy-mlx's fused SDPA kernel streams only ~5 GB/s of KV when one query
# token attends a long context (measured on M1 Max: 8.4 ms at 12k tokens for 24 query / 4 KV
# heads of 256), so generation slowed from 12 to 4.8 tok/s between 1k and 12k of context on
# Qwen3.8-27B. Two matmuls + softmax with the GQA repeat folded into the rows take 2.3 ms for
# the same work. Prompt chunks (L > 1) keep the fused kernel, which is as fast there.
DECODE_COMPOSED_MIN_KV = 1024


def composed_decode_sdpa(queries, keys, values, scale):
    import mlx.core as mx

    B, n_q_heads, L, D = queries.shape
    n_kv_heads, S = keys.shape[1], keys.shape[2]
    R = n_q_heads // n_kv_heads
    q = (queries * scale).reshape(B * n_kv_heads, R * L, D)
    scores = q @ keys.reshape(B * n_kv_heads, S, D).swapaxes(-1, -2)
    probs = mx.softmax(scores.astype(mx.float32), axis=-1).astype(queries.dtype)
    return (probs @ values.reshape(B * n_kv_heads, S, values.shape[-1])).reshape(B, n_q_heads, L, values.shape[-1])


def install_decode_attention():
    """Route single-token decode over a long context through composed_decode_sdpa."""
    if os.environ.get("LMDECK_MLX_DECODE_ATTN", "composed") != "composed":
        return False
    try:
        import mlx_lm.models.base as base
    except Exception:
        return False
    stock = base.scaled_dot_product_attention

    def scaled_dot_product_attention(queries, keys, values, cache, scale, mask, sinks=None):
        if (
            sinks is None
            and not hasattr(cache, "bits")
            and queries.ndim == 4
            and queries.shape[2] == 1
            and keys.shape[2] >= DECODE_COMPOSED_MIN_KV
            and (mask is None or isinstance(mask, str))
            and queries.shape[1] % keys.shape[1] == 0
        ):
            return composed_decode_sdpa(queries, keys, values, scale)
        return stock(queries, keys, values, cache, scale=scale, mask=mask, sinks=sinks)

    base.scaled_dot_product_attention = scaled_dot_product_attention
    # Model modules import the function by name; repoint the ones already loaded.
    for name, module in list(sys.modules.items()):
        if name.startswith("mlx_lm.models.") and getattr(module, "scaled_dot_product_attention", None) is stock:
            module.scaled_dot_product_attention = scaled_dot_product_attention
    return True


def kv_quant_probe(bits, group):
    """True if quantized KV attention runs on this runtime and matches the f16
    reference, else the reason. Checks numbers, not just "it ran": 4-bit on
    omarchy-mlx 0.7.28 runs but returns wrong results."""
    try:
        import mlx.core as mx
        from mlx_lm.models.cache import QuantizedKVCache

        mx.random.seed(0)
        cache = QuantizedKVCache(group_size=group, bits=bits)
        k = mx.random.normal((1, 2, 300, 128)).astype(mx.bfloat16)
        v = mx.random.normal((1, 2, 300, 128)).astype(mx.bfloat16)
        qk, qv = cache.update_and_fetch(k, v)
        q = mx.random.normal((1, 8, 5, 128)).astype(mx.bfloat16)
        scale = 128**-0.5
        got = folded_quantized_sdpa(q, qk, qv, scale, "causal", group, bits)
        ref = mx.fast.scaled_dot_product_attention(
            q,
            mx.dequantize(*qk, group_size=group, bits=bits),
            mx.dequantize(*qv, group_size=group, bits=bits),
            scale=scale,
            mask="causal",
        )
        err = float(mx.abs(got.astype(mx.float32) - ref.astype(mx.float32)).max())
        return True if err < 0.05 else f"wrong results on this runtime (max error {err:.3f})"
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
        try:
            import mlx_lm.models.base as _base

            _base.quantized_scaled_dot_product_attention = folded_quantized_sdpa
        except Exception:
            pass
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


# Optional: prompt-cache checkpoints. The stock single-request path only caches
# prompt+output, which a hybrid model (non-trimmable cache, e.g. Qwen3.5/3.8) can never
# reuse, so every agent turn re-read the whole prompt. lmdeck checkpoints
#   1. the system prompt (tools included), shared by every conversation of the same agent,
#      also kept on disk so it survives restarts and model reloads;
#   2. the conversation up to the answer being generated, for the next turn.
OPTIONAL_PATCHES = [
    (
        "            cache, rest = self.prompt_cache.fetch_nearest_cache(\n"
        "                self.model_provider.model_key, prompt\n"
        "            )\n",
        "            cache, rest = _lmdeck_fetch(self, prompt)\n",
    ),
    (
        "            # Process the prompt and generate tokens\n            for gen in stream_generate(",
        "            if self.model_provider.draft_model is None:\n"
        "                rest = _lmdeck_checkpoint(self, model, tokenizer, prompt, rest, cache, progress)\n"
        "            # Process the prompt and generate tokens\n            for gen in stream_generate(",
    ),
]

MIN_CHECKPOINT = 256
DISK_KEEP = 6


def _disk_dir(server):
    import hashlib

    key = repr(server.model_provider.model_key) + "|" + os.environ.get("LMDECK_MLX_KV_BITS", "0")
    try:
        import importlib.metadata as md

        key += "|" + md.version("mlx-omarchy")
    except Exception:
        pass
    base = os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "lmdeck", "prompt-cache")
    return os.path.join(base, hashlib.sha256(key.encode()).hexdigest()[:16])


def _tok_hash(tokens):
    import hashlib

    return hashlib.sha256(",".join(map(str, tokens)).encode()).hexdigest()


def _im_start(tokenizer):
    try:
        im = tokenizer.convert_tokens_to_ids("<|im_start|>")
    except Exception:
        return None
    return im if isinstance(im, int) and im >= 0 else None


def lmdeck_fetch(server, prompt):
    """In-memory prompt cache lookup, falling back to a system-prompt checkpoint on disk."""
    import copy
    import json

    key = server.model_provider.model_key
    cache, rest = server.prompt_cache.fetch_nearest_cache(key, prompt)
    covered = len(prompt) - len(rest)
    d = _disk_dir(server)
    try:
        metas = [os.path.join(d, f) for f in os.listdir(d) if f.endswith(".json")]
    except OSError:
        return cache, rest
    best = None
    for path in metas:
        try:
            meta = json.load(open(path))
        except Exception:
            continue
        n = int(meta.get("n", 0))
        if covered < n <= len(prompt) and (best is None or n > best[0]) and meta.get("sha") == _tok_hash(prompt[:n]):
            best = (n, path[:-5] + ".safetensors")
    if best is None:
        return cache, rest
    try:
        from mlx_lm.models.cache import load_prompt_cache

        loaded = load_prompt_cache(best[1])
        os.utime(best[1])
    except Exception as e:
        print(f"lmdeck: could not load prompt checkpoint {best[1]}: {e}", file=sys.stderr, flush=True)
        return cache, rest
    server.prompt_cache.insert_cache(key, prompt[: best[0]], loaded, cache_type="system")
    print(f"lmdeck: system prompt restored from disk ({best[0]} tokens)", file=sys.stderr, flush=True)
    return copy.deepcopy(loaded), prompt[best[0] :]


def _save_disk(server, tokens, cache):
    import json

    from mlx_lm.models.cache import save_prompt_cache

    d = _disk_dir(server)
    sha = _tok_hash(tokens)
    path = os.path.join(d, sha[:24])
    if os.path.exists(path + ".json"):
        return
    os.makedirs(d, exist_ok=True)
    save_prompt_cache(path + ".safetensors", cache)
    with open(path + ".json", "w") as f:
        json.dump({"n": len(tokens), "sha": sha}, f)
    # Keep the most recently used few.
    files = sorted(
        (p for p in os.listdir(d) if p.endswith(".safetensors")),
        key=lambda p: os.path.getmtime(os.path.join(d, p)),
        reverse=True,
    )
    for old in files[DISK_KEEP:]:
        for ext in (".safetensors", ".json"):
            try:
                os.remove(os.path.join(d, old[: -len(".safetensors")] + ext))
            except OSError:
                pass


def lmdeck_checkpoint(server, model, tokenizer, prompt, rest, cache, progress):
    """Prefill up to the checkpoint boundaries, saving the cache at each; return what is left."""
    import copy

    import mlx.core as mx

    im = _im_start(tokenizer)
    if im is None:
        return rest
    starts = [i for i, t in enumerate(prompt) if t == im]
    if len(starts) < 2:
        return rest
    system_end, answer_start = starts[1], starts[-1]
    pos = len(prompt) - len(rest)
    step = getattr(server.cli_args, "prefill_step_size", None) or 512
    key = server.model_provider.model_key
    for boundary, kind in ((system_end, "system"), (answer_start, "user")):
        if boundary - pos < MIN_CHECKPOINT:
            continue
        toks = prompt[pos:boundary]
        for i in range(0, len(toks), step):
            chunk = toks[i : i + step]
            model(mx.array(chunk)[None], cache=cache)
            mx.eval([c.state for c in cache])
            progress(pos + i + len(chunk), len(prompt))
        pos = boundary
        snap = copy.deepcopy(cache)
        server.prompt_cache.insert_cache(key, prompt[:boundary], snap, cache_type=kind)
        if kind == "system":
            try:
                _save_disk(server, prompt[:boundary], snap)
            except Exception as e:
                print(f"lmdeck: could not save prompt checkpoint: {e}", file=sys.stderr, flush=True)
    return prompt[pos:]


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
    want_checkpoints = os.environ.get("LMDECK_MLX_CHECKPOINTS", "1") != "0"
    for old, new in OPTIONAL_PATCHES:
        if want_checkpoints and applied == len(PATCHES) and src.count(old) == 1:
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
    mod.__dict__["_lmdeck_fetch"] = lmdeck_fetch
    exec(compile(src, path, "exec"), mod.__dict__)
    # The BatchGenerator path is ~40% slower per stream than stream_generate on
    # this backend; a desktop rarely runs concurrent requests, so default to the
    # single-request path unless batching is asked for.
    batching = os.environ.get("LMDECK_MLX_BATCH") == "1"
    rg = getattr(mod, "ResponseGenerator", None)
    if not batching and rg is not None and hasattr(rg, "_is_batchable"):
        rg._is_batchable = lambda self, args: False
    kv = install_kv_options(mod)
    if install_decode_attention():
        kv += ", composed decode attention"
    print(
        f"lmdeck: mlx_lm.server fast path active (no per-token logprob sync, batching {'on' if batching else 'off'}, {kv}"
        f"{', prefix checkpoints' if checkpoint else ''})",
        file=sys.stderr,
        flush=True,
    )
    return mod.main()


if __name__ == "__main__":
    main()
