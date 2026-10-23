"""Which MLX quantization modes have GPU kernels on this omarchy-mlx runtime.

Checks both quantized matmul and the gather a quantized embedding does, the
two ops a quantized LLM needs. Prints one JSON line."""
import importlib
import json
import pkgutil

import mlx.core as mx

res = {}
x = mx.random.normal((1, 256)).astype(mx.bfloat16)
w = mx.random.normal((256, 256)).astype(mx.bfloat16)
idx = mx.array([1, 5, 7])
for mode, (gs, bits) in {"affine": (64, 4), "mxfp4": (32, 4), "nvfp4": (16, 4), "mxfp8": (32, 8)}.items():
    try:
        q = mx.quantize(w, group_size=gs, bits=bits, mode=mode)
        y = mx.quantized_matmul(x, *q, transpose=True, group_size=gs, bits=bits, mode=mode)
        mx.eval(y, *[mx.take(t, idx, axis=0) for t in q])
        res[mode] = True
    except Exception as e:  # noqa: BLE001
        res[mode] = str(e).splitlines()[0][:200]
info = mx.device_info()
budget = int(info.get("max_recommended_working_set_size") or info.get("memory_size") or 0)


def model_types():
    """model_type values load_model accepts: modules with Model + ModelArgs, plus remaps."""
    try:
        import mlx_lm.models as models

        out = set()
        for m in pkgutil.iter_modules(models.__path__):
            try:
                mod = importlib.import_module(f"mlx_lm.models.{m.name}")
                if hasattr(mod, "Model") and hasattr(mod, "ModelArgs"):
                    out.add(m.name)
            except BaseException:  # noqa: BLE001 - some optional models sys.exit on import
                pass
        try:
            from mlx_lm.utils import MODEL_REMAPPING

            out.update(MODEL_REMAPPING)
        except BaseException:  # noqa: BLE001
            pass
        return sorted(out)
    except BaseException:  # noqa: BLE001
        return []


print(json.dumps({"schema": 2, "mlx": mx.__version__, "modes": res, "gpu_budget": budget, "model_types": model_types()}))
