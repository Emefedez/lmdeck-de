#!/usr/bin/env bash
# Build lmdeck and install it for the current user.
#   ./install.sh            build + install binary, launcher, Omarchy menu rows
#   ./install.sh --service  also enable the user service (server starts at login)
#   ./install.sh --plugin   also install the Omarchy bar panel (replaces omarchy.agents)
#   ./install.sh --vscode   also build + install the VS Code extension (Copilot fallback)
set -euo pipefail
cd "$(dirname "$0")"
command -v cargo >/dev/null || { echo "cargo not found (pacman -S rust)"; exit 1; }
cargo build --release
install -Dm755 target/release/lmdeck "$HOME/.local/bin/lmdeck"
"$HOME/.local/bin/lmdeck" desktop
for arg in "$@"; do
  case "$arg" in
    --service)
      "$HOME/.local/bin/lmdeck" service install
      echo "lmdeck server enabled on http://127.0.0.1:11434"
      ;;
    --plugin)
      src="$PWD/omarchy-plugin/m1-max.agents"
      dst="$HOME/.config/omarchy/plugins/m1-max.agents"
      python3 "$src/sync-from-stock.py"
      if [[ -e "$dst" && ! -L "$dst" ]]; then mv "$dst" "$dst.bak.$(date +%s)"; fi
      ln -sfn "$src" "$dst"
      omarchy bar put m1-max.agents --after omarchy.agents >/dev/null 2>&1 || omarchy bar put m1-max.agents
      omarchy plugin disable omarchy.agents >/dev/null 2>&1 || true
      omarchy restart shell >/dev/null 2>&1 || true
      echo "Omarchy bar: Agents + lmdeck panel installed (stock omarchy.agents disabled)"
      ;;
    --vscode)
      vsix="$(python3 vscode-extension/build-vsix.py)"
      code --install-extension "$vsix" --force
      "$HOME/.local/bin/lmdeck" vscode >/dev/null 2>&1 || true
      echo "VS Code: lmdeck extension installed (status bar: Copilot / local model)"
      ;;
  esac
done
echo "Installed: $HOME/.local/bin/lmdeck  (run 'lmdeck' or find it in the Omarchy menu)"
