import time, mlx.core as mx

def composed(q, k, v, scale):
    B, Hq, L, D = q.shape; Hk = k.shape[1]; S = k.shape[2]; R = Hq // Hk
    qf = (q * scale).reshape(B * Hk, R * L, D)
    s = qf @ k.reshape(B * Hk, S, D).swapaxes(-1, -2)
    p = mx.softmax(s.astype(mx.float32), axis=-1).astype(q.dtype)
    return (p @ v.reshape(B * Hk, S, D)).reshape(B, Hq, L, D)

def bench(fn, *a, n=30):
    for _ in range(3): mx.eval(fn(*a))
    t = time.perf_counter()
    for _ in range(n): mx.eval(fn(*a))
    return (time.perf_counter() - t) / n * 1e3

mx.random.seed(0)
Hq, Hk, D = 24, 4, 256
print("S        fused_ms  composed_ms  max_err   kv_MiB  fused_GB/s")
for S in (1024, 4096, 8192, 12288, 24576):
    q = mx.random.normal((1, Hq, 1, D)).astype(mx.bfloat16)
    # Real caches are slices of a larger buffer; mimic that.
    kb = mx.random.normal((1, Hk, S + 256, D)).astype(mx.bfloat16); vb = mx.random.normal((1, Hk, S + 256, D)).astype(mx.bfloat16)
    k, v = kb[:, :, :S], vb[:, :, :S]; mx.eval(k, v)
    sc = D ** -0.5
    f = bench(lambda: mx.fast.scaled_dot_product_attention(q, k, v, scale=sc))
    c = bench(lambda: composed(q, k, v, sc))
    err = float(mx.abs(mx.fast.scaled_dot_product_attention(q, k, v, scale=sc).astype(mx.float32) - composed(q, k, v, sc).astype(mx.float32)).max())
    kv = 2 * Hk * S * D * 2 / 2**20
    print(f"{S:<8} {f:8.3f}  {c:10.3f}  {err:8.4f}  {kv:7.1f}  {kv/1024/(f/1e3):8.1f}")
