import sys, mlx.core as mx
sys.path.insert(0, ".")
from qsdpa import folded_qsdpa
from mlx_lm.models.base import quantized_scaled_dot_product_attention as stock
from mlx_lm.models.cache import QuantizedKVCache

mx.random.seed(0)
for bits in (8, 4):
    for (Hq, Hk, D, S, L, mask) in [(24, 4, 256, 300, 1, None), (24, 4, 256, 300, 37, "causal"), (16, 2, 128, 1000, 1, None)]:
        cache = QuantizedKVCache(group_size=64, bits=bits)
        k = mx.random.normal((1, Hk, S, D)).astype(mx.bfloat16)
        v = mx.random.normal((1, Hk, S, D)).astype(mx.bfloat16)
        qk, qv = cache.update_and_fetch(k, v)
        q = mx.random.normal((1, Hq, L, D)).astype(mx.bfloat16)
        scale = D ** -0.5
        # Reference: dequantized K/V through the regular fused SDPA.
        dk = mx.dequantize(*qk, group_size=64, bits=bits)
        dv = mx.dequantize(*qv, group_size=64, bits=bits)
        ref = mx.fast.scaled_dot_product_attention(q, dk, dv, scale=scale, mask=mask)
        got = folded_qsdpa(q, qk, qv, scale, mask, 64, bits)
        err = float(mx.abs(got.astype(mx.float32) - ref.astype(mx.float32)).max())
        try:
            s = stock(q, qk, qv, scale=scale, mask=mask, group_size=64, bits=bits); mx.eval(s); stock_ok = "stock ok"
        except Exception as e:
            stock_ok = "stock FAILS: " + str(e).splitlines()[0][:70]
        print(f"bits={bits} Hq={Hq} Hk={Hk} D={D} S={S} L={L} mask={mask}: max|folded-ref|={err:.4f}  ({stock_ok})")
