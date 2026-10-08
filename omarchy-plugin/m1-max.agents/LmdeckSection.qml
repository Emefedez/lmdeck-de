import QtQuick
import Quickshell
import qs.Commons
import qs.Ui

// Live lmdeck view inside the Agents panel, split into themed cards:
// Server · Now · Downloads · KV cache · Memory & residency · Backend & network
// · Models · Hugging Face.
Column {
  id: sec

  property var lm: null
  property color foreground: Color.foreground
  property color dim: Qt.darker(foreground, 1.55)
  property color track: Style.selectedFillFor(foreground, Color.accent)
  property color urgent: Color.urgent
  property string fontFamily: Style.font.family
  readonly property bool searchFocused: searchField.activeFocus

  property string searchKind: "mlx"

  spacing: Style.space(14)

  function alpha(c, a) { return Qt.rgba(c.r, c.g, c.b, a) }
  function setting(key) { return lm && lm.quick ? String(lm.quick[key] || "") : "" }
  function isLoaded(name) {
    for (var i = 0; i < lm.running.length; i++) if (lm.running[i].name === name) return true
    return false
  }
  readonly property bool anyStale: {
    for (var i = 0; i < lm.running.length; i++) if (lm.running[i].stale) return true
    return false
  }
  readonly property bool lanExposed: setting("server.host") === "0.0.0.0"
  readonly property string endpoint: lm && lm.status ? "http://127.0.0.1:" + lm.status.port : lm.baseUrl
  readonly property string lanIp: {
    var ips = lm && lm.status ? (lm.status.ips || []) : []
    for (var i = 0; i < ips.length; i++)
      if (ips[i].iface.indexOf("docker") !== 0 && ips[i].iface.indexOf("veth") !== 0) return ips[i].ip
    return ips.length > 0 ? ips[0].ip : ""
  }

  // ======================= Server =======================
  Card {
    title: "SERVER"
    badge: lm.pending ? (lm.up ? "stopping…" : "starting…") : (lm.up ? "on" : "off")
    badgeColor: lm.up ? Color.accent : sec.dim
    hasSwitch: true
    switchOn: lm.up
    switchBusy: lm.pending
    onSwitchToggled: lm.service(lm.up ? "stop" : "start")

    Caption { text: "Endpoint"; color: sec.dim }
    Text {
      width: parent.width
      textFormat: Text.PlainText
      text: sec.endpoint + "   Ollama · OpenAI /v1"
      color: lm.up ? sec.foreground : sec.dim
      font.family: sec.fontFamily
      font.pixelSize: Style.font.body
      elide: Text.ElideRight
    }
    Text {
      width: parent.width
      visible: lm.up
      textFormat: Text.PlainText
      text: sec.lanExposed
        ? "LAN  http://" + sec.lanIp + ":" + lm.status.port + "  (reachable from your network)"
        : "LAN  " + (sec.lanIp || "no network") + "  ·  local only"
      color: sec.lanExposed ? sec.urgent : sec.dim
      font.family: sec.fontFamily
      font.pixelSize: Style.font.caption
      elide: Text.ElideRight
    }

    Gap {}

    Row {
      spacing: Style.spacing.sm
      SmallButton { visible: lm.up; text: "Restart"; onClicked: lm.service("restart") }
      SmallButton {
        text: "Force stop"
        foreground: sec.urgent
        tooltipText: "SIGKILL the server and every model backend"
        onClicked: lm.service("kill")
      }
      SmallButton { text: "Open TUI"; onClicked: lm.openTui() }
    }

    Text {
      visible: lm.toast !== ""
      width: parent.width
      textFormat: Text.PlainText
      text: lm.toast
      color: Color.accent
      font.family: sec.fontFamily
      font.pixelSize: Style.font.caption
      wrapMode: Text.WordWrap
    }
  }

  // ======================= Now =======================
  Card {
    visible: lm.up
    title: "NOW"

    // --- GPU gap offer ---
    Rectangle {
      visible: !!lm.offer
      width: parent.width
      implicitHeight: offerCol.implicitHeight + Style.space(16)
      radius: Style.cornerRadius
      color: sec.alpha(sec.urgent, 0.10)
      border.width: 1
      border.color: sec.alpha(sec.urgent, 0.35)
      Column {
        id: offerCol
        x: Style.space(8); y: Style.space(8)
        width: parent.width - Style.space(16)
        spacing: Style.space(6)
        Caption {
          width: parent.width
          text: lm.offer ? lm.offer.model + " uses an op omarchy-mlx can't run on the Apple GPU yet." : ""
          color: sec.foreground
          wrapMode: Text.WordWrap
        }
        Flow {
          width: parent.width
          spacing: Style.spacing.sm
          SmallButton {
            text: "Run on CPU (discouraged)"
            tooltipText: "Correct but much slower than the GPU"
            onClicked: lm.setDevice(lm.offer.model, "cpu", true)
          }
          SmallButton {
            visible: !!lm.offer && lm.offer.recommend !== ""
            text: "Download " + (lm.offer ? lm.offer.recommend.split("/").pop() : "")
            tooltipText: lm.offer ? lm.offer.recommend : ""
            onClicked: { lm.pull(lm.offer.recommend); lm.offer = null }
          }
          SmallButton { text: "Dismiss"; onClicked: lm.offer = null }
        }
      }
    }

    // --- loaded models ---
    Caption { text: "Loaded"; color: sec.dim }
    Caption {
      visible: lm.running.length === 0 && !(lm.status && lm.status.load)
      text: "Nothing loaded — models load on first request"
      color: sec.dim
    }
    Column {
      readonly property var l: lm.status ? lm.status.load : null
      visible: !!l
      width: parent.width
      spacing: Style.space(4)
      Caption {
        width: parent.width
        text: {
          var l = parent.l
          if (!l) return ""
          var name = String(l.model).split("/").pop()
          var detail = l.phase === "loading"
            ? "weights " + lm.bytes(l.loaded_bytes) + " / " + lm.bytes(l.total_bytes) + (l.eta_secs ? " · ETA " + Math.round(l.eta_secs) + " s" : "")
            : l.phase === "warming up"
              ? (l.warmup ? "warming up " + l.warmup[0] + " / " + l.warmup[1] + " tokens" : "warming up (compiling GPU kernels)")
              : l.phase === "starting" ? "starting backend" : l.phase
          return "Loading " + name + " — " + detail + " · " + Math.round(l.elapsed_secs) + " s"
        }
        color: Color.accent
        elide: Text.ElideMiddle
      }
      Bar { width: parent.width; value: parent.l ? parent.l.progress : 0 }
    }
    Repeater {
      model: lm.running
      Item {
        required property var modelData
        width: parent.width
        implicitHeight: Math.max(runInfo.implicitHeight, runButtons.implicitHeight)

        Column {
          id: runInfo
          anchors.left: parent.left
          anchors.right: runButtons.left
          anchors.rightMargin: Style.spacing.sm
          anchors.verticalCenter: parent.verticalCenter
          Text {
            width: parent.width
            textFormat: Text.PlainText
            text: modelData.name
            color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.bodySmall
            elide: Text.ElideMiddle
          }
          Caption {
            width: parent.width
            text: modelData.format + (modelData.device === "cpu" ? " · CPU (slow)" : "") + " · " + lm.bytes(modelData.rss) + " RSS · "
              + (modelData.in_flight > 0 ? "generating" : "unloads in " + lm.duration(modelData.expires_in_secs))
            color: sec.dim
            elide: Text.ElideRight
          }
          Caption {
            visible: modelData.stale
            text: "Settings changed — reload to apply"
            color: sec.urgent
          }
          Caption {
            visible: !!modelData.note
            width: parent.width
            text: "⚠ " + (modelData.note || "")
            color: sec.urgent
            wrapMode: Text.WordWrap
          }
        }
        Row {
          id: runButtons
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.spacing.xs
          SmallButton { visible: modelData.device === "cpu"; text: "Use GPU"; onClicked: lm.setDevice(modelData.name, "gpu", false) }
          SmallButton { visible: modelData.stale; text: "Reload"; onClicked: lm.reload(modelData.name) }
          SmallButton { text: "Unload"; onClicked: lm.unload(modelData.name) }
        }
      }
    }

    Divider {}

    // --- memory ---
    Item {
      width: parent.width
      implicitHeight: memLabel.implicitHeight
      readonly property real total: lm.status ? lm.status.mem.total : 0
      readonly property real used: lm.status ? lm.status.mem.total - lm.status.mem.available : 0
      Caption { id: memLabel; text: "Unified memory"; color: sec.dim }
      Caption { anchors.right: parent.right; text: lm.bytes(parent.used) + " / " + lm.bytes(parent.total); color: sec.foreground }
    }
    Bar {
      width: parent.width
      value: lm.status && lm.status.mem.total > 0 ? 1 - lm.status.mem.available / lm.status.mem.total : 0
      alarming: value > 0.9
    }

    Divider {}

    // --- throughput ---
    Caption { text: "Throughput"; color: sec.dim }
    Grid {
      columns: 2
      columnSpacing: Style.space(16)
      rowSpacing: Style.space(4)
      readonly property var last: lm.usageInfo ? lm.usageInfo.last : null
      readonly property var today: lm.usageInfo ? lm.usageInfo.today : null
      Caption { text: "Last request"; color: sec.dim }
      Caption {
        text: parent.last ? Number(parent.last.gen_tps).toFixed(1) + " tok/s · ttft " + Number(parent.last.ttft_secs).toFixed(2) + " s" : "—"
        color: sec.foreground
      }
      Caption { text: "Prefill"; color: sec.dim }
      Caption { text: parent.last && parent.last.prompt_tps > 0 ? Math.round(parent.last.prompt_tps) + " tok/s" : "—"; color: sec.foreground }
      Caption { text: "Today"; color: sec.dim }
      Caption {
        text: parent.today ? parent.today.requests + " req · " + lm.count(parent.today.prompt) + " in · " + lm.count(parent.today.completion) + " out" : "—"
        color: sec.foreground
      }
    }
  }

  // ======================= Downloads =======================
  Card {
    visible: lm.downloads.length > 0
    title: "DOWNLOADS"
    action: "clear"
    onActionClicked: lm.clearDownloads()

    Repeater {
      model: lm.downloads
      Column {
        id: dl
        required property var modelData
        readonly property string state: modelData.status ? modelData.status.state : ""
        readonly property bool active: ["downloading", "resolving", "verifying"].indexOf(state) >= 0
        width: parent.width
        spacing: Style.space(3)
        Item {
          width: parent.width
          implicitHeight: dlName.implicitHeight
          Caption {
            id: dlName
            anchors.left: parent.left
            anchors.right: dlState.left
            anchors.rightMargin: Style.spacing.sm
            text: dl.modelData.name
            color: sec.foreground
            elide: Text.ElideMiddle
          }
          Caption {
            id: dlState
            anchors.right: parent.right
            text: {
              var d = dl.modelData
              if (dl.state === "done") return "✓ " + lm.bytes(d.total)
              if (dl.state === "failed") return "failed"
              if (dl.state === "cancelled") return "cancelled"
              return lm.bytes(d.completed) + "/" + lm.bytes(d.total) + " · " + lm.bytes(d.completed / Math.max(0.1, d.elapsed_secs)) + "/s  ✕"
            }
            color: dl.state === "failed" ? sec.urgent : sec.dim
            MouseArea {
              anchors.fill: parent
              enabled: dl.active
              cursorShape: Qt.PointingHandCursor
              onClicked: lm.cancel(dl.modelData.spec)
            }
          }
        }
        Caption {
          visible: dl.state === "failed"
          width: parent.width
          text: dl.modelData.status ? String(dl.modelData.status.detail || "") : ""
          color: sec.urgent
          wrapMode: Text.WordWrap
          maximumLineCount: 3
        }
        Bar {
          width: parent.width
          value: dl.modelData.total > 0 ? dl.modelData.completed / dl.modelData.total : 0
          alarming: dl.state === "failed"
        }
      }
    }
  }

  // ======================= KV cache =======================
  Card {
    visible: lm.up
    title: "KV CACHE"
    badge: sec.anyStale ? "reload to apply" : ""
    badgeColor: sec.urgent

    Caption { text: "MLX  (Apple GPU)"; color: sec.dim }
    SettingRow {
      label: "Quantization"
      key: "mlx.kv_bits"
      info: "Precision of the attention cache (the model's memory of the conversation), not of the weights. f16: exact, biggest. 8-bit: about half the memory, practically no quality loss. 4-bit: about a quarter, faster on long chats but can drift on long or precise tasks (code, numbers). Weights keep their own quant (the 4bit in the model name)."
      options: [{ value: "0", label: "f16" }, { value: "8", label: "8-bit" }, { value: "4", label: "4-bit" }]
    }
    SettingRow {
      label: "Window"
      key: "mlx.max_kv_size"
      info: "How many tokens of conversation the cache keeps. \u221e keeps everything (memory grows with the chat). A window (4k/8k/32k) caps memory; older tokens are dropped, so the model forgets the start of very long chats."
      options: [{ value: "0", label: "∞" }, { value: "4096", label: "4k" }, { value: "8192", label: "8k" }, { value: "32768", label: "32k" }]
    }
    SettingRow {
      label: "Prompt cache"
      key: "mlx.prompt_cache_size"
      info: "How many previous prompts stay cached so a follow-up that starts the same way (agents, long system prompts) skips re-reading them. More = faster repeats, more RAM; off = always reprocess."
      options: [{ value: "0", label: "off" }, { value: "4", label: "4" }, { value: "10", label: "10" }, { value: "32", label: "32" }]
    }

    Divider {}

    Caption { text: "GGUF  (llama.cpp)"; color: sec.dim }
    SettingRow {
      label: "Type"
      key: "llama.cache_type_k"
      info: "GGUF attention-cache precision. f16: exact. q8_0: half the memory, near-lossless. q4_0: a quarter, some quality loss. Needs flash attention for the V cache."
      options: ["f16", "q8_0", "q4_0"]
      handler: function(v) { lm.setSetting("llama.cache_type_k", v); lm.setSetting("llama.cache_type_v", v) }
    }
    SettingRow {
      label: "Context"
      key: "llama.ctx_size"
      info: "Maximum context for GGUF models. llama.cpp reserves cache for all of it up front, so bigger means more memory even for short chats."
      options: [{ value: "4096", label: "4k" }, { value: "8192", label: "8k" }, { value: "16384", label: "16k" }, { value: "32768", label: "32k" }, { value: "65536", label: "64k" }]
    }
    SettingRow {
      label: "Prefix reuse"
      key: "llama.cache_reuse"
      info: "Reuse cached chunks when a prompt shifts (agent loops re-send history). 256/1k = minimum matching chunk in tokens; off disables it."
      options: [{ value: "0", label: "off" }, { value: "256", label: "256" }, { value: "1024", label: "1k" }]
    }

    SmallButton {
      visible: sec.anyStale
      text: "Reload loaded model to apply"
      onClicked: lm.reload("")
    }
  }

  // ======================= Coding agents =======================
  Card {
    visible: lm.up
    title: "CODING AGENTS"

    SettingRow {
      label: "Tools sent"
      key: "server.local_tools"
      info: "What Claude Code (t1code, T3, lmdeck launch) sends to a local model. lean: essential tools with short descriptions, no MCP tools (~6.6k tokens instead of ~148k). groups: lean plus your MCP tools on demand; the model looks up a group (e.g. rea/inspect) and lmdeck answers from ~/.config/lmdeck/tool-groups, so it only pays for what it uses (answers arrive whole, not streamed). core: every Claude Code tool but no MCP (~17k). all: everything as sent (can be 100k+ tokens: very slow and may run out of memory)."
      options: [{ value: "lean", label: "lean" }, { value: "groups", label: "groups" }, { value: "core", label: "core" }, { value: "all", label: "all" }]
    }
    SettingRow {
      label: "Reuse between turns"
      key: "mlx.prefix_checkpoints"
      info: "Saves the conversation's cache at the start of each answer so the next turn only reads what is new. Needed for hybrid models such as Qwen3.5/3.8, whose cache can't be rewound: an agent follow-up drops from ~99 s to ~13 s. Costs some RAM per saved conversation (bounded by the prompt cache size). Takes effect after a model reload."
      options: [{ value: "true", label: "on" }, { value: "false", label: "off" }]
    }
  }

  // ======================= Memory & residency =======================
  Card {
    visible: lm.up
    title: "MEMORY & RESIDENCY"

    SettingRow {
      label: "Keep loaded"
      key: "server.keep_alive"
      info: "How long an idle model stays loaded. Longer = instant replies, but the memory stays taken; 0 unloads right after each answer; \u221e never unloads."
      options: [{ value: "0", label: "0" }, { value: "5m", label: "5m" }, { value: "30m", label: "30m" }, { value: "2h", label: "2h" }, { value: "-1", label: "∞" }]
    }
    SettingRow { label: "Max models"; key: "server.max_loaded"
      info: "How many models can be resident at once. On 32 GB unified memory, more than one big model risks swapping; the least recently used one is unloaded when needed."; options: ["1", "2", "3"] }
    SettingRow {
      label: "GPU heap"
      key: "server.gpu_heap"
      info: "How much RAM the GPU driver may use for a model. Asahi gives the GPU 50% of RAM by default. auto raises it just for the backend that needs it, capped by free RAM minus the reserves; stock never raises it."
      options: [{ value: "auto", label: "auto", tooltip: "Raise the GPU heap per model only when it needs more than the stock 50% of RAM; capped by free RAM" },
                { value: "stock", label: "stock", tooltip: "Never raise: GPU gets 50% of RAM" }]
    }
    Caption {
      width: parent.width
      visible: !!lm.status
      readonly property var mem: lm.status ? lm.status.mem : null
      text: mem ? "Stock GPU heap " + lm.bytes(mem.gpu_budget) + " · raised per model only, restored when it unloads"
        + (mem.muvm ? " · muvm VM running (" + lm.bytes(mem.muvm) + "): its VM reserve is kept free too" : "") : ""
      color: sec.dim
      wrapMode: Text.WordWrap
    }
    SettingRow {
      label: "RAM check"
      key: "server.mem_check"
      info: "Refuse a load when free RAM can't hold the model plus overhead. Off lets you try anyway (risk of swapping or a frozen desktop)."
      options: [{ value: "true", label: "on" }, { value: "false", label: "off" }]
    }
  }

  // ======================= Backend & network =======================
  Card {
    visible: lm.up
    title: "BACKEND & NETWORK"

    SettingRow {
      label: "MLX fast path"
      key: "mlx.fast_path"
      options: [{ value: "true", label: "on" }, { value: "false", label: "off" }]
    }
    SettingRow {
      label: "Listen"
      key: "server.host"
      options: [{ value: "127.0.0.1", label: "local" }, { value: "0.0.0.0", label: "LAN" }]
      handler: function(v) { lm.setSetting("server.host", v); restartLater.restart() }
    }
    Timer { id: restartLater; interval: 400; onTriggered: lm.service("restart") }

    Divider {}
    SettingRow {
      label: "VS Code Copilot fallback"
      key: "vscode.fallback"
      options: [{ value: "auto", label: "auto", tooltip: "Use a local model for chat and inline Fix while Copilot's quota is exhausted" },
                { value: "always", label: "always" }, { value: "off", label: "off" }]
    }
    Caption {
      width: parent.width
      visible: !!lm.vscode && lm.vscode.installed
      readonly property string active: lm.status && lm.status.vscode_active ? lm.status.vscode_active : ""
      text: !lm.vscode ? "" : (lm.vscode.quota_blocked ? "Copilot quota exhausted · " : "Copilot quota available · ")
        + (active ? "Fix/chat use " + active.split("/").pop() : "VS Code uses Copilot")
        + " · checked by the lmdeck VS Code extension (startup / focus)"
      color: active ? Color.accent : sec.dim
      wrapMode: Text.WordWrap
    }
  }

  // ======================= Models =======================
  Card {
    visible: lm.up
    title: "MODELS"
    badge: lm.models.length + " local"
    badgeColor: sec.dim

    Repeater {
      model: lm.up ? lm.models : []
      Item {
        id: modelRow
        required property var modelData
        readonly property bool loaded: sec.isLoaded(modelData.name)
        width: parent.width
        implicitHeight: modelName.implicitHeight + Style.spacing.md

        Rectangle {
          anchors.fill: parent
          radius: Style.cornerRadius
          color: modelHover.containsMouse ? sec.alpha(sec.foreground, 0.10) : (modelRow.loaded ? sec.alpha(sec.foreground, 0.06) : "transparent")
        }
        Caption {
          id: modelName
          anchors.left: parent.left
          anchors.leftMargin: Style.space(6)
          anchors.right: modelMeta.left
          anchors.rightMargin: Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          readonly property string level: modelData.compat ? modelData.compat.level : ""
          text: (modelRow.loaded ? "● " : "") + modelData.name
            + (level === "unlikely" ? "  ✗" : level === "untested" ? "  ?" : level === "qualified" ? "  ✓" : "")
            + (modelData.device === "cpu" ? "  cpu" : "")
          color: level === "unlikely" ? sec.urgent : sec.foreground
          elide: Text.ElideMiddle
        }
        Caption {
          id: modelMeta
          anchors.right: parent.right
          anchors.rightMargin: Style.space(6)
          anchors.verticalCenter: parent.verticalCenter
          text: modelData.format + " · " + modelData.quant + " · " + lm.bytes(modelData.size)
          color: sec.dim
        }
        MouseArea {
          id: modelHover
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          onClicked: modelRow.loaded ? lm.unload(modelRow.modelData.name) : lm.load(modelRow.modelData.name)
        }
        PanelToolTip {
          visible: modelHover.containsMouse
          text: (modelRow.loaded ? "Click to unload" : "Click to load")
            + (modelData.compat && modelData.compat.reason ? " — " + modelData.compat.reason : "")
            + (modelData.compat && modelData.compat.recommend ? ". Recommended: " + modelData.compat.recommend : "")
          fontFamily: sec.fontFamily
        }
      }
    }
  }

  // ======================= Hugging Face =======================
  Card {
    visible: lm.up
    title: "HUGGING FACE"

    Row {
      width: parent.width
      spacing: Style.spacing.sm
      TextField {
        id: searchField
        width: parent.width - kindGroup.width - parent.spacing
        placeholderText: "Search models…"
        font.family: sec.fontFamily
        font.pixelSize: Style.font.bodySmall
        foreground: sec.foreground
        onAccepted: if (text.trim() !== "") lm.search(text.trim(), sec.searchKind)
        Keys.onEscapePressed: focus = false
      }
      ButtonGroup {
        id: kindGroup
        options: [{ value: "mlx", label: "MLX" }, { value: "gguf", label: "GGUF" }]
        value: sec.searchKind
        foreground: sec.foreground
        fontFamily: sec.fontFamily
        fontSize: Style.font.caption
        focusable: false
        onChanged: function(v) {
          sec.searchKind = v
          if (searchField.text.trim() !== "") lm.search(searchField.text.trim(), v)
        }
      }
    }

    Caption {
      visible: lm.searching || lm.searchError !== ""
      text: lm.searching ? "Searching…" : lm.searchError
      color: lm.searchError !== "" ? sec.urgent : sec.dim
    }

    Repeater {
      model: lm.up ? lm.searchResults : []
      Column {
        id: hit
        required property var modelData
        readonly property bool open: lm.repoId === modelData.id
        width: parent.width
        spacing: Style.space(4)

        Item {
          width: parent.width
          implicitHeight: hitName.implicitHeight + Style.spacing.md
          Rectangle {
            anchors.fill: parent
            radius: Style.cornerRadius
            color: hitHover.containsMouse || hit.open ? sec.alpha(sec.foreground, 0.08) : "transparent"
          }
          Caption {
            id: hitName
            anchors.left: parent.left
            anchors.leftMargin: Style.space(6)
            anchors.right: hitStats.left
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            readonly property string level: hit.modelData.compat ? hit.modelData.compat.level : ""
            text: (hit.open ? "▾ " : "▸ ") + hit.modelData.id
              + (level === "unlikely" ? "  ✗" : level === "untested" ? "  ?" : level === "qualified" ? "  ✓" : "")
            color: level === "unlikely" ? sec.urgent : sec.foreground
            elide: Text.ElideMiddle
          }
          Caption {
            id: hitStats
            anchors.right: parent.right
            anchors.rightMargin: Style.space(6)
            anchors.verticalCenter: parent.verticalCenter
            text: "⬇ " + lm.count(hit.modelData.downloads) + "  ♥ " + lm.count(hit.modelData.likes)
            color: sec.dim
          }
          MouseArea {
            id: hitHover
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: lm.openRepo(hit.modelData.id)
          }
        }

        Caption {
          visible: hit.open && lm.repoLoading
          leftPadding: Style.space(18)
          text: "Listing files…"
          color: sec.dim
        }

        Column {
          readonly property var v: hit.open ? lm.repoCompat : null
          visible: !!v && v.level !== "ok"
          x: Style.space(18)
          width: hit.width - Style.space(18)
          spacing: Style.space(4)
          Caption {
            width: parent.width
            text: !parent.v ? "" : parent.v.level === "unlikely" ? "✗ Likely won't run on the Apple GPU"
              : parent.v.level === "untested" ? "? Untested on omarchy-mlx" : "✓ Qualified on omarchy-mlx"
            color: parent.v && parent.v.level === "unlikely" ? sec.urgent : parent.v && parent.v.level === "qualified" ? Color.accent : sec.foreground
            font.bold: true
          }
          Caption {
            visible: !!parent.v && parent.v.level !== "qualified" && parent.v.reason !== ""
            width: parent.width
            text: parent.v ? parent.v.reason : ""
            color: sec.dim
            wrapMode: Text.WordWrap
          }
          SmallButton {
            visible: !!parent.v && !!parent.v.recommend
            text: "Get recommended: " + (parent.v && parent.v.recommend ? parent.v.recommend.split("/").pop() : "")
            tooltipText: parent.v && parent.v.recommend ? parent.v.recommend : ""
            onClicked: lm.openRepo(parent.v.recommend)
          }
        }

        Repeater {
          model: hit.open ? lm.repoChoices : []
          Item {
            id: choice
            required property var modelData
            width: hit.width
            implicitHeight: choiceText.implicitHeight + Style.spacing.sm
            Rectangle {
              anchors.fill: parent
              anchors.leftMargin: Style.space(14)
              radius: Style.cornerRadius
              color: choiceHover.containsMouse ? sec.alpha(sec.foreground, 0.10) : "transparent"
            }
            Caption {
              id: choiceText
              anchors.left: parent.left
              anchors.leftMargin: Style.space(20)
              anchors.verticalCenter: parent.verticalCenter
              text: choice.modelData.label + (choice.modelData.default ? " ★" : "")
              color: sec.foreground
            }
            Caption {
              anchors.right: parent.right
              anchors.rightMargin: Style.space(6)
              anchors.verticalCenter: parent.verticalCenter
              text: lm.bytes(choice.modelData.size) + " · " + choice.modelData.fit + "  ⤓"
              color: choice.modelData.fit === "too big" ? sec.urgent : sec.dim
            }
            MouseArea {
              id: choiceHover
              anchors.fill: parent
              hoverEnabled: true
              cursorShape: Qt.PointingHandCursor
              onClicked: lm.pull(choice.modelData.spec)
            }
          }
        }
      }
    }
  }

  // ======================= components =======================

  // A themed chunk: tinted rounded surface, header (+ optional badge or
  // action link), and a column of content.
  component Card: Rectangle {
    id: card
    property string title: ""
    property string badge: ""
    property color badgeColor: sec.dim
    property string action: ""
    signal actionClicked()
    // Optional on/off switch at the right of the header (replaces the badge).
    property bool hasSwitch: false
    property bool switchOn: false
    property bool switchBusy: false
    signal switchToggled()
    default property alias content: body.data
    readonly property real pad: Style.space(12)

    width: sec.width
    implicitHeight: head.height + body.implicitHeight + pad * 2 + Style.space(8)
    radius: Style.cornerRadius
    color: sec.alpha(sec.foreground, 0.035)
    border.width: 1
    border.color: sec.alpha(sec.foreground, 0.09)

    Item {
      id: head
      x: card.pad
      y: card.pad
      width: card.width - card.pad * 2
      height: Math.max(headTitle.implicitHeight, card.hasSwitch ? headSwitch.implicitHeight : 0)
      PanelSectionHeader { id: headTitle; anchors.verticalCenter: parent.verticalCenter; text: card.title; foreground: sec.foreground; fontFamily: sec.fontFamily }
      ToggleSwitch {
        id: headSwitch
        visible: card.hasSwitch
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        checked: card.switchOn
        busy: card.switchBusy
        foreground: sec.foreground
        onToggled: card.switchToggled()
      }
      Caption {
        visible: card.badge !== "" || card.action !== ""
        anchors.right: card.hasSwitch ? headSwitch.left : parent.right
        anchors.rightMargin: card.hasSwitch ? Style.space(10) : 0
        anchors.verticalCenter: parent.verticalCenter
        text: card.action !== "" ? card.action : card.badge
        color: card.action !== "" ? sec.dim : card.badgeColor
        MouseArea {
          anchors.fill: parent
          enabled: card.action !== ""
          cursorShape: Qt.PointingHandCursor
          onClicked: card.actionClicked()
        }
      }
    }

    Column {
      id: body
      x: card.pad
      y: head.y + head.height + Style.space(8)
      width: card.width - card.pad * 2
      spacing: Style.space(8)
    }
  }

  component Caption: Text {
    textFormat: Text.PlainText
    font.family: sec.fontFamily
    font.pixelSize: Style.font.caption
  }

  component Divider: Rectangle {
    width: parent ? parent.width : 0
    height: 1
    color: sec.alpha(sec.foreground, 0.08)
  }

  component Gap: Item { width: 1; height: Style.space(2) }

  component SmallButton: Button {
    bordered: true
    foreground: sec.foreground
    fontFamily: sec.fontFamily
    fontSize: Style.font.caption
  }

  component Bar: Item {
    property real value: 0
    property bool alarming: false
    implicitHeight: Math.max(Style.space(4), Math.round(Style.spacing.controlHeight * 0.14))
    Rectangle { anchors.fill: parent; radius: height / 2; color: sec.track }
    Rectangle {
      anchors.left: parent.left
      height: parent.height
      radius: height / 2
      width: parent.width * Math.max(0, Math.min(1, parent.value))
      color: parent.alarming ? sec.urgent : sec.foreground
    }
  }

  component SettingRow: Column {
    id: row
    property string label: ""
    property string key: ""
    property var options: []
    property var handler: null
    // What changing this does; shown under the row when the ⓘ is clicked.
    property string info: ""
    property bool infoOpen: false
    width: parent ? parent.width : 0
    spacing: 2
    Item {
      width: parent.width
      implicitHeight: Math.max(rowLabel.implicitHeight, group.implicitHeight)
      Row {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        spacing: 4
        Caption {
          id: rowLabel
          text: row.label
          color: sec.foreground
        }
        Caption {
          visible: row.info !== ""
          text: "ⓘ"
          color: row.infoOpen ? sec.foreground : sec.dim
          MouseArea {
            anchors.fill: parent
            anchors.margins: -3
            cursorShape: Qt.PointingHandCursor
            onClicked: row.infoOpen = !row.infoOpen
          }
        }
      }
      ButtonGroup {
        id: group
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        options: row.options
        value: sec.setting(row.key)
        foreground: sec.foreground
        fontFamily: sec.fontFamily
        fontSize: Style.font.caption
        focusable: false
        onChanged: function(v) { if (row.handler) row.handler(v); else lm.setSetting(row.key, v) }
      }
    }
    Caption {
      visible: row.infoOpen && row.info !== ""
      width: parent.width
      text: row.info
      color: sec.dim
      wrapMode: Text.WordWrap
    }
  }
}
