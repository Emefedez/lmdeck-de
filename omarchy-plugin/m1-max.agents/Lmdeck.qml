import QtQuick
import Quickshell

// lmdeck daemon client. Cost model, because this lives in the bar:
//   - panel closed, server up:   one long-poll request parked in the daemon,
//                                answered only when something changes (≤ 1/min idle)
//   - panel closed, server down: one refused localhost connect every 20 s
//   - panel open:                a status poll every 1.5 s for live progress
// No processes are spawned except for explicit button presses.
Item {
  id: lm
  visible: false

  property string baseUrl: "http://127.0.0.1:11434"
  property bool panelOpen: false

  property bool up: false
  property var status: null
  property var models: []
  property var config: []
  property var vscode: null
  property var searchResults: []
  property bool searching: false
  property string searchError: ""
  property string repoId: ""
  property var repoChoices: []
  property var repoCompat: null
  // Set when a load failed for lack of a GPU kernel: { model, recommend }.
  property var offer: null
  property bool repoLoading: false
  property string toast: ""
  // A start/stop was requested and the server hasn't changed state yet.
  property bool pending: false
  property bool _pendingFrom: false

  property int _event: 0
  property var _watchXhr: null

  readonly property var running: status ? (status.running || []) : []
  readonly property var downloads: status ? (status.downloads || []) : []
  readonly property var usageInfo: status ? (status.usage || null) : null
  readonly property var quick: status ? (status.settings || ({})) : ({})
  readonly property bool busy: {
    if (!status) return false
    if (status.loading) return true
    for (var i = 0; i < running.length; i++) if (running[i].in_flight > 0) return true
    return false
  }

  function request(method, path, body, cb, timeoutMs) {
    var xhr = new XMLHttpRequest()
    xhr.onreadystatechange = function() {
      if (xhr.readyState !== XMLHttpRequest.DONE) return
      var data = null
      try { data = xhr.responseText ? JSON.parse(xhr.responseText) : null } catch (e) { data = xhr.responseText }
      if (cb) cb(xhr.status, data)
    }
    xhr.open(method, baseUrl + path)
    if (timeoutMs) xhr.timeout = timeoutMs
    if (body !== undefined && body !== null) {
      xhr.setRequestHeader("Content-Type", "application/json")
      xhr.send(JSON.stringify(body))
    } else {
      xhr.send()
    }
    return xhr
  }

  function applyStatus(code, data) {
    var nowUp = code === 200 && data && typeof data === "object"
    if (lm.pending && nowUp !== lm._pendingFrom) lm.pending = false
    if (code === 200 && data && typeof data === "object") {
      var wasUp = up
      var changed = data.event !== _event
      lm.up = true
      lm.status = data
      lm._event = data.event
      if (!wasUp || (changed && panelOpen)) refreshModels()
      return true
    }
    lm.up = false
    lm.status = null
    return false
  }

  // ---- long-poll loop (always on, nearly free) ----
  function watch() {
    if (_watchXhr) return
    lm._watchXhr = request("GET", "/lmdeck/watch?since=" + _event + "&timeout=55", null, function(code, data) {
      lm._watchXhr = null
      if (applyStatus(code, data)) Qt.callLater(watch)
      else retryTimer.restart()
    }, 70000)
  }

  Timer { id: retryTimer; interval: 20000; onTriggered: lm.watch() }

  // ---- live poll only while the panel is visible ----
  Timer {
    interval: 1500
    repeat: true
    running: lm.panelOpen && lm.up
    onTriggered: lm.request("GET", "/lmdeck/status", null, lm.applyStatus, 3000)
  }

  onPanelOpenChanged: if (panelOpen) {
    request("GET", "/lmdeck/status", null, applyStatus, 3000)
    refreshModels()
    refreshVscode()
    if (config.length === 0) refreshConfig()
  }

  Component.onCompleted: watch()

  function refreshModels() {
    request("GET", "/lmdeck/models", null, function(code, data) { if (code === 200 && Array.isArray(data)) lm.models = data })
  }
  function refreshVscode() {
    request("GET", "/lmdeck/vscode", null, function(code, data) { if (code === 200) lm.vscode = data })
  }
  function refreshConfig() {
    request("GET", "/lmdeck/config", null, function(code, data) { if (code === 200 && Array.isArray(data)) lm.config = data })
  }

  function say(text) { lm.toast = text; toastTimer.restart() }
  Timer { id: toastTimer; interval: 5000; onTriggered: lm.toast = "" }

  function errorText(data, fallback) {
    return data && data.error ? String(data.error) : fallback
  }

  // ---- actions ----
  function load(name) {
    say("Loading " + name + "…")
    request("POST", "/lmdeck/load", { model: name }, function(code, data) {
      if (code === 200) { lm.offer = null; lm.say(name + " ready (" + Number(data.load_secs).toFixed(1) + " s)"); return }
      if (data && data.gpu_gap) { lm.offer = { model: data.model || name, recommend: data.recommend || "" }; lm.say(""); return }
      lm.say(lm.errorText(data, "load failed"))
    })
  }
  function setDevice(name, device, thenLoad) {
    request("POST", "/lmdeck/override", { model: name, key: "device", value: device }, function(code, data) {
      if (code !== 200) { lm.say(lm.errorText(data, "could not change device")); return }
      lm.offer = null
      lm.refreshModels()
      if (thenLoad) lm.load(name)
      else lm.say(name + (device === "cpu" ? " will run on the CPU (discouraged)" : " back on the Apple GPU"))
    })
  }
  function unload(name) {
    request("POST", "/lmdeck/unload", name ? { model: name } : {}, function(code, data) {
      if (code !== 200) say(errorText(data, "unload failed"))
    })
  }
  function reload(name) {
    say("Reloading to apply settings…")
    request("POST", "/lmdeck/restart", name ? { model: name } : {}, function(code, data) {
      say(code === 200 ? "Reloaded" : errorText(data, "reload failed"))
    })
  }
  function setSetting(key, value) {
    request("POST", "/lmdeck/config", { key: key, value: String(value) }, function(code, data) {
      if (code !== 200) { say(errorText(data, "could not save")); return }
      if (key.indexOf("vscode.") === 0) Qt.callLater(lm.refreshVscode)
      if (data.restart_server) say("Saved — restart the server to apply")
      else if (data.reload) say("Saved — reload the model to apply")
      request("GET", "/lmdeck/status", null, lm.applyStatus, 3000)
    })
  }
  function pull(spec) {
    say("Downloading " + spec)
    request("POST", "/lmdeck/pull", { model: spec }, function(code, data) {
      if (code !== 200) say(errorText(data, "pull failed"))
    })
  }
  function cancel(spec) { request("POST", "/lmdeck/cancel", { model: spec }, null) }
  function clearDownloads() { request("DELETE", "/lmdeck/downloads", null, null) }

  function search(query, kind) {
    lm.searching = true
    lm.searchError = ""
    lm.repoId = ""
    lm.repoChoices = []
    request("GET", "/lmdeck/search?limit=25&kind=" + kind + "&q=" + encodeURIComponent(query), null, function(code, data) {
      lm.searching = false
      if (code === 200 && Array.isArray(data)) lm.searchResults = data
      else { lm.searchResults = []; lm.searchError = lm.errorText(data, "search failed") }
    }, 20000)
  }
  function openRepo(id) {
    if (repoId === id) { lm.repoId = ""; lm.repoChoices = []; lm.repoCompat = null; return }
    lm.repoId = id
    lm.repoChoices = []
    lm.repoCompat = null
    lm.repoLoading = true
    request("GET", "/lmdeck/repo?id=" + encodeURIComponent(id), null, function(code, data) {
      if (lm.repoId !== id) return
      lm.repoLoading = false
      if (code === 200 && data && Array.isArray(data.choices)) { lm.repoChoices = data.choices; lm.repoCompat = data.compat || null }
      else lm.say(lm.errorText(data, "could not list files"))
    }, 20000)
  }

  // Service control goes through systemd: no daemon round trip needed, and a
  // force stop also takes the backends (same cgroup) down with it.
  function service(action) {
    var cmd = {
      start: ["systemctl", "--user", "start", "lmdeck"],
      stop: ["systemctl", "--user", "stop", "lmdeck"],
      restart: ["systemctl", "--user", "restart", "lmdeck"],
      // SIGKILL the whole cgroup, then stop so Restart=on-failure doesn't revive it.
      kill: ["sh", "-c", "systemctl --user kill -s KILL lmdeck; systemctl --user stop lmdeck"]
    }[action]
    if (!cmd) return
    Quickshell.execDetached(cmd)
    if (action === "start" || action === "stop") {
      lm.pending = true
      lm._pendingFrom = lm.up
      pendingTimeout.restart()
    }
    say(action === "kill" ? "Force-stopped lmdeck and its backends" : "lmdeck: " + action)
    if (action === "kill") { lm.up = false; lm.status = null }
    if (_watchXhr) { _watchXhr.abort(); lm._watchXhr = null }
    retryTimer.interval = 1500
    retryTimer.restart()
    retryReset.restart()
  }
  Timer { id: retryReset; interval: 8000; onTriggered: retryTimer.interval = 20000 }
  Timer { id: pendingTimeout; interval: 15000; onTriggered: lm.pending = false }

  function openTui() {
    Quickshell.execDetached(["omarchy-launch-or-focus-tui", "lmdeck"])
  }

  // ---- formatting ----
  function bytes(n) {
    n = Number(n || 0)
    if (n >= 1073741824) return (n / 1073741824).toFixed(1) + "G"
    if (n >= 1048576) return Math.round(n / 1048576) + "M"
    if (n >= 1024) return Math.round(n / 1024) + "K"
    return n + "B"
  }
  function count(n) {
    n = Number(n || 0)
    if (n >= 1e9) return (n / 1e9).toFixed(1) + "B"
    if (n >= 1e6) return (n / 1e6).toFixed(1) + "M"
    if (n >= 1e3) return (n / 1e3).toFixed(1) + "k"
    return String(n)
  }
  function duration(s) {
    if (s === null || s === undefined) return "∞"
    s = Number(s)
    if (s >= 3600) return Math.floor(s / 3600) + "h" + Math.floor((s % 3600) / 60) + "m"
    if (s >= 60) return Math.floor(s / 60) + "m"
    return s + "s"
  }
}
