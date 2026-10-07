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


def main():
    path = _orig.__file__
    src = open(path, encoding="utf-8").read()
    applied = 0
    for old, new in PATCHES:
        if src.count(old) == 1:
            src = src.replace(old, new)
            applied += 1
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
    exec(compile(src, path, "exec"), mod.__dict__)
    # The BatchGenerator path is ~40% slower per stream than stream_generate on
    # this backend; a desktop rarely runs concurrent requests, so default to the
    # single-request path unless batching is asked for.
    batching = os.environ.get("LMDECK_MLX_BATCH") == "1"
    rg = getattr(mod, "ResponseGenerator", None)
    if not batching and rg is not None and hasattr(rg, "_is_batchable"):
        rg._is_batchable = lambda self, args: False
    print(
        f"lmdeck: mlx_lm.server fast path active (no per-token logprob sync, batching {'on' if batching else 'off'})",
        file=sys.stderr,
        flush=True,
    )
    return mod.main()


if __name__ == "__main__":
    main()
