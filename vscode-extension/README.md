# lmdeck for VS Code

When your Copilot quota is exhausted, Copilot chat and inline chat ("Fix") switch to a local model served by [lmdeck](https://github.com/Emefedez/lmdeck). They switch back when your quota returns, and your previous settings are restored exactly.

- **Status bar.** Shows `$(copilot) Copilot` or the local model in use. Click it for the menu: check now, Auto / Always local / Off, and the automatic-check toggle.
- **No polling.** The extension checks only when VS Code starts and when the window regains focus (at most every `lmdeck.focusCheckMinutes`, default 15), or when you ask. Each check is one request to the local lmdeck server.
- **Shared mode.** The mode is lmdeck's `vscode.fallback` setting, so the lmdeck TUI and the Omarchy bar panel show and change the same value.
