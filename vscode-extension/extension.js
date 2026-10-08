// lmdeck for VS Code: when Copilot's quota is exhausted, point chat and
// inline chat ("Fix") at a local model served by lmdeck, and switch back
// when quota returns.
//
// Cost model: no timers. A check is one HTTP call to the local lmdeck
// daemon (which reads VS Code's recorded quota state) and happens only at
// startup, when the window regains focus (rate-limited), or on request.
// The mode (auto / always / off) lives in lmdeck's config, so the TUI and
// the Omarchy bar panel show and change the same setting.

const vscode = require("vscode");
const { execFile } = require("child_process");

const KEYS = ["inlineChat.defaultModel", "chat.defaultModel"];
const SAVED = "lmdeck.savedDefaults"; // { key: previousGlobalValue | null }
const ACTIVE = "lmdeck.activeModel";

let ctx;
let item;
let out;

function log(msg) {
  if (out) out.appendLine(`${new Date().toISOString()} ${msg}`);
}
let lastCheck = 0;
let last = null; // last daemon answer

function cfg() {
  return vscode.workspace.getConfiguration("lmdeck");
}

function base() {
  return String(cfg().get("url") || "http://127.0.0.1:11434").replace(/\/$/, "");
}

async function api(method, path, body) {
  const res = await fetch(base() + path, {
    method,
    headers: body ? { "content-type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(5000),
  });
  if (!res.ok) throw new Error(`lmdeck ${path}: HTTP ${res.status}`);
  return res.json();
}

function qualified(model) {
  // How Copilot names models from an Ollama-type provider: "<name> (ollama)".
  return `${model} (ollama)`;
}

async function applyLocal(model) {
  const settings = vscode.workspace.getConfiguration();
  const saved = ctx.globalState.get(SAVED) || {};
  for (const key of KEYS) {
    const current = settings.inspect(key)?.globalValue;
    const ours = qualified(model);
    if (current === ours) continue;
    // Remember what the user had, once, so it can be restored exactly.
    if (!(key in saved) && current !== ctx.globalState.get(ACTIVE + ":" + key)) {
      saved[key] = current === undefined ? null : current;
    }
    await settings.update(key, ours, vscode.ConfigurationTarget.Global);
    await ctx.globalState.update(ACTIVE + ":" + key, ours);
  }
  await ctx.globalState.update(SAVED, saved);
  await ctx.globalState.update(ACTIVE, model);
}

async function restore() {
  const settings = vscode.workspace.getConfiguration();
  const saved = ctx.globalState.get(SAVED) || {};
  for (const key of KEYS) {
    const ours = ctx.globalState.get(ACTIVE + ":" + key);
    const current = settings.inspect(key)?.globalValue;
    // Only undo what we set; if the user changed it since, leave it.
    if (ours !== undefined && current === ours) {
      const prev = key in saved ? saved[key] : null;
      await settings.update(key, prev === null ? undefined : prev, vscode.ConfigurationTarget.Global);
    }
    await ctx.globalState.update(ACTIVE + ":" + key, undefined);
  }
  await ctx.globalState.update(SAVED, undefined);
  await ctx.globalState.update(ACTIVE, undefined);
}

async function check(reason) {
  lastCheck = Date.now();
  try {
    last = await api("GET", "/lmdeck/vscode");
    log(`check (${reason}): mode=${last.mode} quota_blocked=${last.quota_blocked} suggested=${last.suggested_model}`);
  } catch (e) {
    log(`check (${reason}) failed: ${e.message}`);
    last = null;
    render(`lmdeck not reachable (${e.message})`);
    return;
  }
  const want =
    last.mode === "always" || (last.mode === "auto" && last.quota_blocked === true);
  const model = want ? last.suggested_model : null;
  const active = ctx.globalState.get(ACTIVE);
  if (model && model !== active) {
    try {
      await applyLocal(model);
      log(`applied ${model}`);
    } catch (e) {
      log(`could not apply ${model}: ${e.message}`);
    }
    if (reason !== "startup" || last.quota_blocked) {
      vscode.window.setStatusBarMessage(
        `lmdeck: Fix and chat now use ${model.split("/").pop()}` +
          (last.quota_blocked ? " (Copilot quota exhausted)" : ""),
        6000
      );
    }
  } else if (!model && active) {
    try {
      await restore();
      log("restored Copilot defaults");
    } catch (e) {
      log(`could not restore: ${e.message}`);
    }
    vscode.window.setStatusBarMessage("lmdeck: back to Copilot", 5000);
  }
  try {
    await api("POST", "/lmdeck/vscode/state", { active_model: ctx.globalState.get(ACTIVE) || null });
  } catch (e) {
    log(`report to lmdeck failed: ${e.message}`);
  }
  render();
}

function render(error) {
  const active = ctx.globalState.get(ACTIVE);
  const auto = cfg().get("autoCheck");
  if (error) {
    item.text = "$(debug-disconnect) lmdeck";
    item.tooltip = error;
  } else if (active) {
    item.text = `$(server-environment) ${active.split("/").pop()}`;
    item.tooltip =
      `Fix and chat use ${active} via lmdeck` +
      (last && last.quota_blocked ? " — Copilot quota exhausted" : "") +
      `\nMode: ${last ? last.mode : "?"} · automatic check ${auto ? "on" : "off"}`;
  } else {
    item.text = "$(copilot) Copilot";
    item.tooltip =
      `lmdeck fallback ${last ? last.mode : "?"}` +
      (last && last.quota_blocked === false ? " · Copilot quota available" : "") +
      ` · automatic check ${auto ? "on" : "off"}`;
  }
  if (!auto) item.text += " $(circle-slash)";
  item.show();
}

async function setMode(mode) {
  try {
    await api("POST", "/lmdeck/config", { key: "vscode.fallback", value: mode });
  } catch (e) {
    vscode.window.showErrorMessage(`lmdeck: ${e.message}`);
    return;
  }
  await check("mode");
}

async function serverUp() {
  try {
    await api("GET", "/api/version");
    return true;
  } catch {
    return false;
  }
}

function systemctl(action) {
  return new Promise((resolve) =>
    execFile("systemctl", ["--user", action, "lmdeck"], (err) => resolve(!err))
  );
}

async function toggleServer() {
  const up = await serverUp();
  const ok = await systemctl(up ? "stop" : "start");
  vscode.window.setStatusBarMessage(
    ok ? `lmdeck server ${up ? "stopped" : "starting…"}` : "lmdeck: systemctl failed (is the user service installed?)",
    5000
  );
  log(`server ${up ? "stop" : "start"} -> ${ok ? "ok" : "failed"}`);
  if (!up && ok) setTimeout(() => check("server-start"), 2500);
  else render(up ? "lmdeck server stopped" : undefined);
}

async function menu() {
  const auto = cfg().get("autoCheck");
  const mode = last ? last.mode : "?";
  const up = await serverUp();
  const pick = await vscode.window.showQuickPick(
    [
      { label: up ? "$(debug-stop) Stop lmdeck server" : "$(debug-start) Start lmdeck server", description: up ? "running on " + base() : "stopped", id: "server" },
      { label: "$(refresh) Check Copilot quota now", id: "check" },
      { label: "$(output) Show log", id: "log" },
      { label: `${mode === "auto" ? "$(check)" : "$(blank)"} Auto`, description: "local model only while Copilot quota is exhausted", id: "auto" },
      { label: `${mode === "always" ? "$(check)" : "$(blank)"} Always local`, description: "Fix and chat always use lmdeck", id: "always" },
      { label: `${mode === "off" ? "$(check)" : "$(blank)"} Off`, description: "always Copilot", id: "off" },
      { label: `$(${auto ? "circle-slash" : "play"}) ${auto ? "Turn off" : "Turn on"} automatic check`, description: "startup + window focus, no polling", id: "toggle" },
    ],
    { title: "lmdeck: Copilot fallback" }
  );
  if (!pick) return;
  if (pick.id === "server") await toggleServer();
  else if (pick.id === "check") await check("manual");
  else if (pick.id === "log") out.show();
  else if (pick.id === "toggle") await toggleAutoCheck();
  else await setMode(pick.id);
}

async function toggleAutoCheck() {
  const next = !cfg().get("autoCheck");
  await cfg().update("autoCheck", next, vscode.ConfigurationTarget.Global);
  vscode.window.setStatusBarMessage(`lmdeck: automatic quota check ${next ? "on" : "off"}`, 4000);
  if (next) await check("toggle");
  else render();
}

function activate(context) {
  ctx = context;
  out = vscode.window.createOutputChannel("lmdeck");
  context.subscriptions.push(out);
  item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 101);
  item.command = "lmdeck.menu";
  context.subscriptions.push(
    item,
    vscode.commands.registerCommand("lmdeck.menu", menu),
    vscode.commands.registerCommand("lmdeck.toggleServer", toggleServer),
    vscode.commands.registerCommand("lmdeck.checkNow", () => check("manual")),
    vscode.commands.registerCommand("lmdeck.toggleAutoCheck", toggleAutoCheck),
    vscode.commands.registerCommand("lmdeck.useLocal", () => setMode("always")),
    vscode.commands.registerCommand("lmdeck.useAuto", () => setMode("auto")),
    vscode.commands.registerCommand("lmdeck.useCopilot", () => setMode("off")),
    vscode.window.onDidChangeWindowState((s) => {
      if (!s.focused || !cfg().get("autoCheck")) return;
      const minutes = Math.max(1, Number(cfg().get("focusCheckMinutes")) || 15);
      if (Date.now() - lastCheck >= minutes * 60000) check("focus");
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("lmdeck")) render();
    })
  );
  if (cfg().get("autoCheck")) check("startup");
  else render();
}

function deactivate() {}

module.exports = { activate, deactivate };
