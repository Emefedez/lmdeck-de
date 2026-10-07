#!/usr/bin/env bash
# Build lmdeck and install it for the current user.
#   ./install.sh            build + install binary, launcher, Omarchy menu rows
#   ./install.sh --service  also enable the user service (server starts at login)
set -euo pipefail
cd "$(dirname "$0")"
command -v cargo >/dev/null || { echo "cargo not found (pacman -S rust)"; exit 1; }
cargo build --release
install -Dm755 target/release/lmdeck "$HOME/.local/bin/lmdeck"
"$HOME/.local/bin/lmdeck" desktop
if [[ "${1:-}" == "--service" ]]; then
  "$HOME/.local/bin/lmdeck" service install
  echo "lmdeck server enabled on http://127.0.0.1:11434"
fi
echo "Installed: $HOME/.local/bin/lmdeck  (run 'lmdeck' or find it in the Omarchy menu)"
