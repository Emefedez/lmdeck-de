# Setup

The steps are in the order they depend on each other. Steps 3 to 5 can each be skipped: lmdeck works with any one
backend.

## 1. The machine

- Asahi Linux on Apple Silicon, with the Apple GPU's Vulkan driver (Honeykrisp, in Mesa). `vulkaninfo --summary`
  should list an Apple device.
- [Omarchy](https://github.com/omacom/omarchy) is optional. Without it you get no menu rows or bar panel, and you
  install packages with pacman yourself.
- Rust (`pacman -S rust`) to build lmdeck.

## 2. lmdeck

```bash
git clone https://github.com/Emefedez/lmdeck-de && cd lmdeck-de
./install.sh             # ~/.local/bin/lmdeck, a launcher entry, Omarchy menu rows
./install.sh --service   # also: the server as a systemd user service that starts at login
./install.sh --plugin    # also: the Omarchy bar panel (replaces the stock Agents widget)
./install.sh --vscode    # also: the VS Code extension (local model when the Copilot quota runs out)
```

The server listens on `127.0.0.1:11434`, Ollama's port, so stop Ollama first if it runs. Settings live in
`~/.config/lmdeck/config.toml`. lmdeck writes only the keys you change, so comments and hand edits stay.

## 3. MLX (Apple GPU)

TUI: **Backends** (4) → MLX → **Install the Omarchy packages**. This is the same as:

```bash
lmdeck update        # 1 = Omarchy packages (runs `omarchy update` first), 2 = the latest GitHub release
```

- The packages are `omarchy-mlx` and `omarchy-mlx-vulkan`, from Omarchy's repo. On Omarchy for Mac that repo is
  sync-only, so lmdeck installs the signed packages with `pacman -U`, and pacman still verifies the signatures.
- The Custom release installs into `~/.local/share/mlx-omarchy` and can be newer than the package. Both can be
  installed at once. **Use the … runtime** chooses which one lmdeck uses. Updating either one never changes that
  choice.
- An old `curl … install.sh` setup is moved to the packages by **Move the script install to the packages** (or
  `lmdeck migrate`).

## 4. GGUF (llama.cpp, optional)

TUI: **Backends** → GGUF → **Build llama.cpp**, or `lmdeck llama build`. It builds `llama-server` with Vulkan into
`~/.local/share/lmdeck/llama.cpp`, at `llama.git_ref` (default `master`). If `llama.server_path` points at your own
binary, lmdeck keeps using that one.

## 5. Strata (optional, 125B MoE)

Needs 32 GB of RAM, about 40 GB of free disk for the Q2_0 model and its streaming copy, and a fast SSD.

1. TUI: **Backends** → Strata → **Install Strata**, or `lmdeck strata install`. It:
   - clones [strata-asahi](https://github.com/Emefedez/strata-asahi) at `vulkan-asahi` into
     `~/.local/share/lmdeck/strata` (`strata.dir`). An update only fast-forwards, and it stops if the checkout has
     local changes;
   - sets up the server's Python venv;
   - builds [honeykrisp-strata](https://github.com/Emefedez/honeykrisp-strata)'s driver into
     `~/.local/lib/honeykrisp-strata/v3` if it is missing. This is a private ICD: only Strata's processes use it,
     and the desktop keeps its Mesa. The build needs meson, clang, llvm, libclc, spirv-llvm-translator,
     spirv-tools, glslang and python-mako/packaging/yaml, and it installs any that are missing;
   - downloads Slang 2026.18.2 into `~/.local/share/slang` and builds the engine (`build-vulkan/strata`).
2. TUI: **Backends** → Strata → **Set up a Strata model**. Strata's setup asks which model to set up, then
   downloads it, packs it, writes the experts' streaming copy and the MTP draft layer, and writes the run config
   `strata-<model>.json`. On btrfs it marks the model folder no-copy-on-write. An existing run config is kept as a
   `.bak` copy.
3. The model shows up as `strata/<model>` in Models. Loading it starts Strata's server with the private driver and
   an expert arena sized from free memory.

Before a Strata run, close what holds the GPU, and do not suspend the machine while the model runs (see
[LIMITATIONS.md](LIMITATIONS.md)).

## 6. Models

**Get models** (3) shows the omarchy-mlx catalog (models qualified on this GPU backend) and searches Hugging Face
for MLX, GGUF or both, with an estimated memory need and fit for each variant. Downloads go to the standard Hugging
Face cache (`~/.cache/huggingface/hub`), resume after an interruption, and are checked with SHA-256. Gated repos
need a token in `~/.cache/huggingface/token` or `HF_TOKEN`. From the shell:

```bash
lmdeck search "qwen 4b" --gguf
lmdeck pull mlx-community/Qwen3.5-4B-MLX-4bit
lmdeck pull unsloth/Qwen3.5-4B-GGUF:Q4_K_M
```

## 7. Clients

```bash
OLLAMA_HOST=http://127.0.0.1:11434                         # Ollama clients
OPENAI_BASE_URL=http://127.0.0.1:11434/v1 OPENAI_API_KEY=lmdeck   # OpenAI clients
lmdeck launch claude -m <model>                            # Claude Code on a local model
lmdeck launch codex -m <model>                             # Codex
```

See [REFERENCE.md](REFERENCE.md) for the endpoints and what lmdeck changes for coding agents.
