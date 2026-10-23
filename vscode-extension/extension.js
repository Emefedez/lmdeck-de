// lmdeck for VS Code: when Copilot's quota is exhausted, point chat and
// inline chat ("Fix") at a local model served by lmdeck, and switch back
// when quota returns.
//
// Cost model: no timers. A check is one HTTP call to the local lmdeck
// daemon (which reads VS Code's recorded quota state) and happens only at
// startup, when the window regains focus (rate-limited), or on request.
// Background quota checks and user data reporting are strictly opt-in.
// The mode (auto / always / off) and target models live in lmdeck's config,
// so the editor UI, TUI, and Omarchy bar panel show and change the same settings.

const vscode = require("vscode");
const { execFile } = require("child_process");

const INLINE_KEY = "inlineChat.defaultModel";
const CHAT_KEY = "chat.defaultModel";
const KEYS = [INLINE_KEY, CHAT_KEY];
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

function isDataCollectionOptedIn() {
  return Boolean(cfg().get("collectUserData"));
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

async function applyLocal(inlineModel, chatModel) {
  const settings = vscode.workspace.getConfiguration();
  const saved = ctx.globalState.get(SAVED) || {};
  const targets = {
    [INLINE_KEY]: inlineModel,
    [CHAT_KEY]: chatModel,
  };
  for (const key of KEYS) {
    const model = targets[key];
    if (!model) continue;
    const current = settings.inspect(key)?.globalValue;
    const ours = qualified(model);
    if (current === ours) continue;
    // Remember what the user had, once, so it can be restored exactly.
    if (!(key in saved) && current !== ctx.globalState.get(ACTIVE + ":" + key)) {
      saved[key] = current === undefined ? null : current;
    }
    await settings.update(key, ours, vscode.ConfigurationTarget.Global);
    await ctx.globalState.update(ACTIVE + ":" + key, ours);
    log(`applied ${model} for ${key}`);
  }
  await ctx.globalState.update(SAVED, saved);
  const activeDesc = inlineModel === chatModel
    ? inlineModel
    : `${chatModel} (chat) · ${inlineModel} (Fix)`;
  await ctx.globalState.update(ACTIVE, activeDesc);
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
    log(`check (${reason}): mode=${last.mode} quota_blocked=${last.quota_blocked} suggested=${last.suggested_model} inline=${last.inline_model} chat=${last.chat_model}`);
  } catch (e) {
    log(`check (${reason}) failed: ${e.message}`);
    last = null;
    render(`lmdeck not reachable (${e.message})`);
    return;
  }
  const want =
    last.mode === "always" || (last.mode === "auto" && last.quota_blocked === true);
  const inlineTarget = want ? (last.inline_model || last.suggested_model) : null;
  const chatTarget = want ? (last.chat_model || last.suggested_model) : null;
  const activeInline = ctx.globalState.get(ACTIVE + ":" + INLINE_KEY);
  const activeChat = ctx.globalState.get(ACTIVE + ":" + CHAT_KEY);
  const inlineQualified = inlineTarget ? qualified(inlineTarget) : null;
  const chatQualified = chatTarget ? qualified(chatTarget) : null;

  if (want && (inlineTarget || chatTarget)) {
    if (inlineQualified !== activeInline || chatQualified !== activeChat) {
      try {
        await applyLocal(inlineTarget, chatTarget);
      } catch (e) {
        log(`could not apply models: ${e.message}`);
      }
      if (reason !== "startup" || last.quota_blocked) {
        const msg = inlineTarget === chatTarget
          ? `lmdeck: Fix and chat now use ${chatTarget.split("/").pop()}`
          : `lmdeck: Fix uses ${inlineTarget.split("/").pop()}, chat uses ${chatTarget.split("/").pop()}`;
        vscode.window.setStatusBarMessage(
          msg + (last.quota_blocked ? " (Copilot quota exhausted)" : ""),
          6000
        );
      }
    }
  } else if (!want && (activeInline || activeChat)) {
    try {
      await restore();
      log("restored Copilot defaults");
    } catch (e) {
      log(`could not restore: ${e.message}`);
    }
    vscode.window.setStatusBarMessage("lmdeck: back to Copilot", 5000);
  }

  // Report state only if user explicitly opted in to user data collection
  if (isDataCollectionOptedIn()) {
    try {
      await api("POST", "/lmdeck/vscode/state", { active_model: ctx.globalState.get(ACTIVE) || null });
    } catch (e) {
      log(`report to lmdeck failed: ${e.message}`);
    }
  }
  render();
}

function render(error) {
  const active = ctx.globalState.get(ACTIVE);
  const backgroundOk = cfg().get("autoCheck") && isDataCollectionOptedIn();
  if (error) {
    item.text = "$(debug-disconnect) lmdeck";
    item.tooltip = error;
  } else if (active) {
    const parts = active.split(" · ");
    const shortText = parts.length > 1
      ? `${parts[0].split("/").pop().replace(" (chat)", "")}/${parts[1].split("/").pop().replace(" (Fix)", "")}`
      : active.split("/").pop();
    item.text = `$(server-environment) ${shortText}`;
    item.tooltip =
      `${active} via lmdeck` +
      (last && last.quota_blocked ? " — Copilot quota exhausted" : "") +
      `\nMode: ${last ? last.mode : "?"} · background check ${backgroundOk ? "on" : "off (opt-in)"}`;
  } else {
    item.text = "$(copilot) Copilot";
    item.tooltip =
      `lmdeck fallback ${last ? last.mode : "?"}` +
      (last && last.quota_blocked === false ? " · Copilot quota available" : "") +
      ` · background check ${backgroundOk ? "on" : "off (opt-in)"}`;
  }
  if (!backgroundOk) item.text += " $(circle-slash)";
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

async function chooseModel(targetKey, title) {
  let models = [];
  try {
    models = await api("GET", "/lmdeck/models");
  } catch (e) {
    log(`could not fetch models: ${e.message}`);
  }
  const toolModels = Array.isArray(models) ? models.filter((m) => m.tools) : [];
  const current = (last && last[targetKey === "vscode.inline_model" ? "inline_model" : "chat_model"]) || "";
  const items = [
    {
      label: "$(sparkle) Automatic / default",
      description: "Fallback to vscode.model",
      id: "",
    },
  ];
  for (const m of toolModels) {
    const isCurrent = m.name === current;
    items.push({
      label: `${isCurrent ? "$(check) " : "$(symbol-misc) "}${m.name}`,
      description: `${m.format} · ${(m.size / (1024 * 1024 * 1024)).toFixed(1)} GB`,
      id: m.name,
    });
  }
  items.push({
    label: "$(pencil) Custom model name…",
    description: "Enter a model name manually",
    id: "__custom__",
  });

  const pick = await vscode.window.showQuickPick(items, { title });
  if (!pick) return;
  let chosen = pick.id;
  if (chosen === "__custom__") {
    const input = await vscode.window.showInputBox({
      prompt: `Enter model name for ${title}`,
      value: current,
    });
    if (input === undefined) return;
    chosen = input.trim();
  }
  try {
    await api("POST", "/lmdeck/config", { key: targetKey, value: chosen });
    log(`set ${targetKey} = "${chosen}"`);
    vscode.window.setStatusBarMessage(`lmdeck: ${title} set to ${chosen || "(auto)"}`, 4000);
  } catch (e) {
    vscode.window.showErrorMessage(`lmdeck: failed to save setting: ${e.message}`);
  }
  await check("model-change");
}

async function toggleCollectUserData() {
  const next = !isDataCollectionOptedIn();
  await cfg().update("collectUserData", next, vscode.ConfigurationTarget.Global);
  vscode.window.setStatusBarMessage(
    `lmdeck: background user data collection ${next ? "enabled (opted in)" : "disabled"}`,
    4000
  );
  if (next) await check("opt-in");
  else render();
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
  const dataOptIn = isDataCollectionOptedIn();
  const mode = last ? last.mode : "?";
  const up = await serverUp();
  const inlineDisplay = (last && last.inline_model ? last.inline_model.split("/").pop() : "(auto)");
  const chatDisplay = (last && last.chat_model ? last.chat_model.split("/").pop() : "(auto)");

  const items = [
    { label: up ? "$(debug-stop) Stop lmdeck server" : "$(debug-start) Start lmdeck server", description: up ? "running on " + base() : "stopped", id: "server" },
    { label: "$(refresh) Check Copilot quota now", id: "check" },
    { label: "$(output) Show log", id: "log" },
    { label: `${mode === "auto" ? "$(check)" : "$(blank)"} Auto`, description: "local model only while Copilot quota is exhausted", id: "auto" },
    { label: `${mode === "always" ? "$(check)" : "$(blank)"} Always local`, description: "Fix and chat always use lmdeck", id: "always" },
    { label: `${mode === "off" ? "$(check)" : "$(blank)"} Off`, description: "always Copilot", id: "off" },
    { label: `$(wrench) Inline Fix model: ${inlineDisplay}`, description: "Choose model for Copilot inline chat / Fix", id: "setInlineModel" },
    { label: `$(comment-discussion) Chat model: ${chatDisplay}`, description: "Choose model for Copilot chat", id: "setChatModel" },
    { label: `${dataOptIn ? "$(check)" : "$(circle-slash)"} Background user data collection (opt-in)`, description: dataOptIn ? "Opted in: background quota check on window focus & usage sync are on" : "Off: no background quota check or data collection", id: "toggleUserData" },
  ];
  if (dataOptIn) {
    items.push({
      label: `$(${auto ? "check" : "circle-slash"}) Automatic quota check on window focus`,
      description: auto ? "Checks quota when window regains focus" : "Off: only manual checks",
      id: "toggleAutoCheck",
    });
  }

  const pick = await vscode.window.showQuickPick(items, { title: "lmdeck: Copilot fallback" });
  if (!pick) return;
  if (pick.id === "server") await toggleServer();
  else if (pick.id === "check") await check("manual");
  else if (pick.id === "log") out.show();
  else if (pick.id === "setInlineModel") await chooseModel("vscode.inline_model", "Fix / inline model");
  else if (pick.id === "setChatModel") await chooseModel("vscode.chat_model", "Chat model");
  else if (pick.id === "toggleUserData") await toggleCollectUserData();
  else if (pick.id === "toggleAutoCheck") await toggleAutoCheck();
  else await setMode(pick.id);
}

async function toggleAutoCheck() {
  const next = !cfg().get("autoCheck");
  await cfg().update("autoCheck", next, vscode.ConfigurationTarget.Global);
  vscode.window.setStatusBarMessage(`lmdeck: automatic quota check ${next ? "on" : "off"}`, 4000);
  if (next && isDataCollectionOptedIn()) await check("toggle");
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
    vscode.commands.registerCommand("lmdeck.toggleCollectUserData", toggleCollectUserData),
    vscode.commands.registerCommand("lmdeck.setInlineModel", () => chooseModel("vscode.inline_model", "Fix / inline model")),
    vscode.commands.registerCommand("lmdeck.setChatModel", () => chooseModel("vscode.chat_model", "Chat model")),
    vscode.commands.registerCommand("lmdeck.useLocal", () => setMode("always")),
    vscode.commands.registerCommand("lmdeck.useAuto", () => setMode("auto")),
    vscode.commands.registerCommand("lmdeck.useCopilot", () => setMode("off")),
    vscode.window.onDidChangeWindowState((s) => {
      if (!s.focused || !cfg().get("autoCheck") || !isDataCollectionOptedIn()) return;
      const minutes = Math.max(1, Number(cfg().get("focusCheckMinutes")) || 15);
      if (Date.now() - lastCheck >= minutes * 60000) check("focus");
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("lmdeck")) render();
    })
  );
  if (cfg().get("autoCheck") && isDataCollectionOptedIn()) check("startup");
  else render();
}

function deactivate() {}

module.exports = { activate, deactivate };
