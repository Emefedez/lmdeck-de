import QtQuick
import Quickshell
import qs.Commons
import qs.Ui

// Live lmdeck view inside the Agents panel: endpoint, loaded models, memory,
// throughput, downloads, cache settings, the local model list, and a
// Hugging Face search that downloads through the daemon.
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
  property bool showSettings: false

  spacing: Style.space(10)

  function alpha(c, a) { return Qt.rgba(c.r, c.g, c.b, a) }
  function setting(key) { return lm && lm.quick ? String(lm.quick[key] || "") : "" }
  function isLoaded(name) {
    for (var i = 0; i < lm.running.length; i++) if (lm.running[i].name === name) return true
    return false
  }
  readonly property bool lanExposed: setting("server.host") === "0.0.0.0"
  readonly property string endpoint: lm && lm.status ? "http://" + (lanExposed ? "127.0.0.1" : lm.status.host) + ":" + lm.status.port : lm.baseUrl
  readonly property string lanIp: {
    var ips = lm && lm.status ? (lm.status.ips || []) : []
    for (var i = 0; i < ips.length; i++) if (ips[i].iface.indexOf("docker") !== 0 && ips[i].iface.indexOf("veth") !== 0) return ips[i].ip
    return ips.length > 0 ? ips[0].ip : ""
  }

  // ---------- server ----------
  PanelSectionHeader { width: parent.width; text: "SERVER"; foreground: sec.foreground; fontFamily: sec.fontFamily }

  Text {
    width: parent.width
    textFormat: Text.PlainText
    text: lm.up
      ? "● " + sec.endpoint + "   (Ollama + OpenAI /v1)"
      : "○ stopped"
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
      ? "LAN  http://" + sec.lanIp + ":" + lm.status.port + "  — reachable from your network"
      : "LAN  " + (sec.lanIp || "no network") + "  · local only"
    color: sec.lanExposed ? sec.urgent : sec.dim
    font.family: sec.fontFamily
    font.pixelSize: Style.font.caption
    elide: Text.ElideRight
  }

  Row {
    spacing: Style.spacing.sm
    Button {
      text: lm.up ? "Restart" : "Start"
      bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.bodySmall
      onClicked: lm.service(lm.up ? "restart" : "start")
    }
    Button {
      visible: lm.up
      text: "Stop"
      bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.bodySmall
      onClicked: lm.service("stop")
    }
    Button {
      text: "Force stop"
      tooltipText: "SIGKILL the server and every model backend"
      bordered: true; foreground: sec.urgent; fontFamily: sec.fontFamily; fontSize: Style.font.bodySmall
      onClicked: lm.service("kill")
    }
    Button {
      text: "Open TUI"
      bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.bodySmall
      onClicked: lm.openTui()
    }
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

  // ---------- loaded + memory + throughput ----------
  Column {
    visible: lm.up
    width: parent.width
    spacing: Style.space(8)

    PanelSeparator { foreground: sec.foreground }
    PanelSectionHeader { width: parent.width; text: "LOADED"; foreground: sec.foreground; fontFamily: sec.fontFamily }

    Text {
      visible: lm.running.length === 0 && !(lm.status && lm.status.loading)
      text: "Nothing loaded — models load on first request"
      color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
    }
    Text {
      visible: !!(lm.status && lm.status.loading)
      width: parent.width
      textFormat: Text.PlainText
      text: lm.status && lm.status.loading ? "Loading " + lm.status.loading + "…" : ""
      color: Color.accent; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
      elide: Text.ElideMiddle
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
          Text {
            width: parent.width
            textFormat: Text.PlainText
            text: modelData.format + " · " + lm.bytes(modelData.rss) + " RSS · "
              + (modelData.in_flight > 0 ? "generating" : "unloads in " + lm.duration(modelData.expires_in_secs))
              + (modelData.stale ? " · settings changed" : "")
            color: modelData.stale ? sec.urgent : sec.dim
            font.family: sec.fontFamily; font.pixelSize: Style.font.caption
            elide: Text.ElideRight
          }
        }
        Row {
          id: runButtons
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.spacing.xs
          Button {
            visible: modelData.stale
            text: "Reload"; bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.caption
            onClicked: lm.reload(modelData.name)
          }
          Button {
            text: "Unload"; bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.caption
            onClicked: lm.unload(modelData.name)
          }
        }
      }
    }

    // Unified memory: what the desktop + models use right now.
    Item {
      width: parent.width
      implicitHeight: memLabel.implicitHeight
      readonly property real total: lm.status ? lm.status.mem.total : 0
      readonly property real used: lm.status ? lm.status.mem.total - lm.status.mem.available : 0
      Text {
        id: memLabel
        text: "Memory"
        color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.bodySmall
      }
      Text {
        anchors.right: parent.right
        textFormat: Text.PlainText
        text: lm.bytes(parent.used) + " / " + lm.bytes(parent.total)
        color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
      }
    }
    Bar {
      width: parent.width
      value: lm.status && lm.status.mem.total > 0 ? 1 - lm.status.mem.available / lm.status.mem.total : 0
      alarming: value > 0.9
    }

    Text {
      width: parent.width
      textFormat: Text.PlainText
      readonly property var last: lm.usageInfo ? lm.usageInfo.last : null
      text: last
        ? "Last: " + Number(last.gen_tps).toFixed(1) + " tok/s"
          + (last.prompt_tps > 0 ? " · prefill " + Math.round(last.prompt_tps) + " tok/s" : "")
          + " · ttft " + Number(last.ttft_secs).toFixed(2) + " s"
        : "No requests yet"
      color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
      elide: Text.ElideRight
    }
    Text {
      width: parent.width
      textFormat: Text.PlainText
      readonly property var today: lm.usageInfo ? lm.usageInfo.today : null
      text: today ? "Today: " + today.requests + " requests · " + lm.count(today.prompt) + " in · " + lm.count(today.completion) + " out" : ""
      color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
    }
  }

  // ---------- downloads ----------
  Column {
    visible: lm.downloads.length > 0
    width: parent.width
    spacing: Style.space(6)

    PanelSeparator { foreground: sec.foreground }
    Item {
      width: parent.width
      implicitHeight: dlHeader.implicitHeight
      PanelSectionHeader { id: dlHeader; text: "DOWNLOADS"; foreground: sec.foreground; fontFamily: sec.fontFamily }
      Text {
        anchors.right: parent.right
        text: "clear"
        color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
        MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: lm.clearDownloads() }
      }
    }
    Repeater {
      model: lm.downloads
      Column {
        id: dl
        required property var modelData
        readonly property string state: modelData.status ? modelData.status.state : ""
        readonly property bool active: ["downloading", "resolving", "verifying"].indexOf(state) >= 0
        width: sec.width
        spacing: Style.space(3)
        Item {
          width: parent.width
          implicitHeight: dlName.implicitHeight
          Text {
            id: dlName
            anchors.left: parent.left
            anchors.right: dlState.left
            anchors.rightMargin: Style.spacing.sm
            textFormat: Text.PlainText
            text: dl.modelData.name
            color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
            elide: Text.ElideMiddle
          }
          Text {
            id: dlState
            anchors.right: parent.right
            textFormat: Text.PlainText
            text: {
              var d = dl.modelData
              if (dl.state === "done") return "✓ " + lm.bytes(d.total)
              if (dl.state === "failed") return "failed"
              if (dl.state === "cancelled") return "cancelled"
              return lm.bytes(d.completed) + "/" + lm.bytes(d.total) + " · " + lm.bytes(d.completed / Math.max(0.1, d.elapsed_secs)) + "/s  ✕"
            }
            color: dl.state === "failed" ? sec.urgent : sec.dim
            font.family: sec.fontFamily; font.pixelSize: Style.font.caption
            MouseArea {
              anchors.fill: parent
              enabled: dl.active
              cursorShape: Qt.PointingHandCursor
              onClicked: lm.cancel(dl.modelData.spec)
            }
          }
        }
        Text {
          visible: dl.state === "failed"
          width: parent.width
          textFormat: Text.PlainText
          text: dl.modelData.status ? String(dl.modelData.status.detail || "") : ""
          color: sec.urgent; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
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

  // ---------- cache & memory settings ----------
  PanelSeparator { visible: lm.up; foreground: sec.foreground }
  Item {
    visible: lm.up
    width: parent.width
    implicitHeight: setHeader.implicitHeight
    PanelSectionHeader { id: setHeader; text: "CACHE & MEMORY"; foreground: sec.foreground; fontFamily: sec.fontFamily }
    Text {
      anchors.right: parent.right
      text: sec.showSettings ? "hide ▴" : "show ▾"
      color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
    }
    MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: sec.showSettings = !sec.showSettings }
  }

  Column {
    visible: lm.up && sec.showSettings
    width: parent.width
    spacing: Style.space(8)

    SettingRow {
      label: "Keep loaded"
      key: "server.keep_alive"
      options: [{ value: "0", label: "0" }, { value: "5m", label: "5m" }, { value: "30m", label: "30m" }, { value: "2h", label: "2h" }, { value: "-1", label: "∞" }]
    }
    SettingRow {
      label: "Max models"
      key: "server.max_loaded"
      options: ["1", "2", "3"]
    }
    SettingRow {
      label: "MLX prompt cache"
      key: "mlx.prompt_cache_size"
      options: [{ value: "0", label: "off" }, { value: "4", label: "4" }, { value: "10", label: "10" }, { value: "32", label: "32" }]
    }
    SettingRow {
      label: "GGUF context"
      key: "llama.ctx_size"
      options: [{ value: "4096", label: "4k" }, { value: "8192", label: "8k" }, { value: "16384", label: "16k" }, { value: "32768", label: "32k" }, { value: "65536", label: "64k" }]
    }
    SettingRow {
      label: "GGUF KV cache"
      key: "llama.cache_type_k"
      options: ["f16", "q8_0", "q4_0"]
      handler: function(v) { lm.setSetting("llama.cache_type_k", v); lm.setSetting("llama.cache_type_v", v) }
    }
    SettingRow {
      label: "KV reuse"
      key: "llama.cache_reuse"
      options: [{ value: "0", label: "off" }, { value: "256", label: "256" }, { value: "1024", label: "1k" }]
    }
    SettingRow {
      label: "MLX fast path"
      key: "mlx.fast_path"
      options: [{ value: "true", label: "on" }, { value: "false", label: "off" }]
    }
    SettingRow {
      label: "Memory check"
      key: "server.mem_check"
      options: [{ value: "true", label: "on" }, { value: "false", label: "off" }]
    }
    SettingRow {
      label: "Listen"
      key: "server.host"
      options: [{ value: "127.0.0.1", label: "local" }, { value: "0.0.0.0", label: "LAN" }]
      handler: function(v) { lm.setSetting("server.host", v); restartLater.restart() }
    }
    Timer { id: restartLater; interval: 400; onTriggered: lm.service("restart") }

    Button {
      visible: {
        for (var i = 0; i < lm.running.length; i++) if (lm.running[i].stale) return true
        return false
      }
      text: "Reload loaded model to apply"
      bordered: true; foreground: sec.foreground; fontFamily: sec.fontFamily; fontSize: Style.font.bodySmall
      onClicked: lm.reload("")
    }
  }

  // ---------- local models ----------
  PanelSeparator { visible: lm.up; foreground: sec.foreground }
  PanelSectionHeader { visible: lm.up; width: parent.width; text: "MODELS (" + lm.models.length + ")"; foreground: sec.foreground; fontFamily: sec.fontFamily }

  Repeater {
    model: lm.up ? lm.models : []
    Item {
      id: modelRow
      required property var modelData
      readonly property bool loaded: sec.isLoaded(modelData.name)
      width: sec.width
      implicitHeight: modelName.implicitHeight + Style.spacing.md

      Rectangle {
        anchors.fill: parent
        radius: Style.cornerRadius
        color: modelHover.containsMouse ? sec.alpha(sec.foreground, 0.10) : (modelRow.loaded ? sec.alpha(sec.foreground, 0.06) : "transparent")
      }
      Text {
        id: modelName
        anchors.left: parent.left
        anchors.leftMargin: Style.space(6)
        anchors.right: modelMeta.left
        anchors.rightMargin: Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.PlainText
        text: (modelRow.loaded ? "● " : "") + modelData.name
        color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
        elide: Text.ElideMiddle
      }
      Text {
        id: modelMeta
        anchors.right: parent.right
        anchors.rightMargin: Style.space(6)
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.PlainText
        text: modelData.format + " · " + modelData.quant + " · " + lm.bytes(modelData.size)
        color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
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
        text: modelRow.loaded ? "Click to unload" : "Click to load"
        fontFamily: sec.fontFamily
      }
    }
  }

  // ---------- Hugging Face search ----------
  PanelSeparator { visible: lm.up; foreground: sec.foreground }
  PanelSectionHeader { visible: lm.up; width: parent.width; text: "HUGGING FACE"; foreground: sec.foreground; fontFamily: sec.fontFamily }

  Row {
    visible: lm.up
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

  Text {
    visible: lm.up && (lm.searching || lm.searchError !== "")
    text: lm.searching ? "Searching…" : lm.searchError
    color: lm.searchError !== "" ? sec.urgent : sec.dim
    font.family: sec.fontFamily; font.pixelSize: Style.font.caption
  }

  Repeater {
    model: lm.up ? lm.searchResults : []
    Column {
      id: hit
      required property var modelData
      readonly property bool open: lm.repoId === modelData.id
      width: sec.width
      spacing: Style.space(4)

      Item {
        width: parent.width
        implicitHeight: hitName.implicitHeight + Style.spacing.md
        Rectangle {
          anchors.fill: parent
          radius: Style.cornerRadius
          color: hitHover.containsMouse || hit.open ? sec.alpha(sec.foreground, 0.08) : "transparent"
        }
        Text {
          id: hitName
          anchors.left: parent.left
          anchors.leftMargin: Style.space(6)
          anchors.right: hitStats.left
          anchors.rightMargin: Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          textFormat: Text.PlainText
          text: (hit.open ? "▾ " : "▸ ") + hit.modelData.id
          color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
          elide: Text.ElideMiddle
        }
        Text {
          id: hitStats
          anchors.right: parent.right
          anchors.rightMargin: Style.space(6)
          anchors.verticalCenter: parent.verticalCenter
          text: "⬇ " + lm.count(hit.modelData.downloads) + "  ♥ " + lm.count(hit.modelData.likes)
          color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
        }
        MouseArea {
          id: hitHover
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          onClicked: lm.openRepo(hit.modelData.id)
        }
      }

      Text {
        visible: hit.open && lm.repoLoading
        leftPadding: Style.space(18)
        text: "Listing files…"
        color: sec.dim; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
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
          Text {
            id: choiceText
            anchors.left: parent.left
            anchors.leftMargin: Style.space(20)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            text: choice.modelData.label + (choice.modelData.default ? " ★" : "")
            color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
          }
          Text {
            anchors.right: parent.right
            anchors.rightMargin: Style.space(6)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            text: lm.bytes(choice.modelData.size) + " · " + choice.modelData.fit + "  ⤓"
            color: choice.modelData.fit === "too big" ? sec.urgent : sec.dim
            font.family: sec.fontFamily; font.pixelSize: Style.font.caption
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

  // ---------- components ----------
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

  component SettingRow: Item {
    id: row
    property string label: ""
    property string key: ""
    property var options: []
    property var handler: null
    width: sec.width
    implicitHeight: Math.max(rowLabel.implicitHeight, group.implicitHeight)
    Text {
      id: rowLabel
      anchors.left: parent.left
      anchors.verticalCenter: parent.verticalCenter
      text: row.label
      color: sec.foreground; font.family: sec.fontFamily; font.pixelSize: Style.font.caption
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
}
