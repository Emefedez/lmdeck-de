#!/usr/bin/env python3
"""Regenerate Panel.qml from Omarchy's stock agents panel plus the lmdeck hooks.

Run after an Omarchy update changes /usr/share/omarchy/shell/plugins/agents/Panel.qml.
The data layer (Main.qml, Agent.qml, collectors) is imported from the stock
directory at runtime, so only the panel is forked.
"""
import pathlib, sys

STOCK = pathlib.Path("/usr/share/omarchy/shell/plugins/agents")
HERE = pathlib.Path(__file__).resolve().parent

s = (STOCK / "Panel.qml").read_text()

def sub(old, new, count=1):
    global s
    if s.count(old) < 1:
        sys.exit(f"stock Panel.qml changed; cannot find:\n{old}")
    s = s.replace(old, new, count)

sub('import qs.Ui\n', 'import qs.Ui\nimport "file://' + str(STOCK) + '" as Stock\n')
sub('moduleName: "omarchy.agents"', 'moduleName: "m1-max.agents"')
sub('ipcTarget: "omarchy.agents"', 'ipcTarget: "m1-max.agents"')

# Providers: stock records plus lmdeck (synthetic tab while no record exists yet), in the picker's order.
sub('readonly property var providers: usage.enabledProviders',
    '''readonly property var providers: {
    var list = (usage.enabledProviders || []).slice()
    var at = -1
    for (var i = 0; i < list.length; i++) if (list[i].providerId === "lmdeck") at = i
    var rec = at >= 0 ? list.splice(at, 1)[0] : null
    if ((rec || lmdeck.up) && root.providerShown("lmdeck")) {
      var p = rec || { providerId: "lmdeck", providerName: "lmdeck", limits: [], recentDays: [], modelUsage: ({}),
                       tierLabel: "", usageStatusText: "", authHelpText: "", hasPromptStats: true }
      list.push(p)
    }
    return root.ordered(list, function(item) { return item.providerId })
  }
  readonly property bool lmdeckSelected: !!provider && provider.providerId === "lmdeck"

  // ---------------------------------------------------------------- picker
  //
  // Which providers the panel shows. The widget's shell.json settings stay the
  // base and ~/.config/omarchy/agents/providers.json (written by the picker)
  // wins, so a toggle survives restarts without rewriting shell.json. A hidden
  // provider is also skipped by the collectors.
  readonly property string extraTool: String(Qt.resolvedUrl("collectors/agents-extra")).replace("file://", "")
  readonly property string prefsPath: (Quickshell.env("XDG_CONFIG_HOME") || (Quickshell.env("HOME") + "/.config")) + "/omarchy/agents/providers.json"
  property var providerPrefs: ({})
  property var providerOrder: []
  property bool pickerExpanded: false
  readonly property bool pickerOpen: pickerExpanded || providers.length === 0
  // Agents Omarchy ships no mark for; ours live in assets/ with -light twins.
  readonly property var forkMarks: ["antigravity", "copilot"]
  readonly property var knownProviders: [
    { id: "claude", name: "Claude" },
    { id: "codex", name: "Codex" },
    { id: "copilot", name: "Copilot" },
    { id: "antigravity", name: "Antigravity" },
    { id: "fireworks", name: "Fireworks" },
    { id: "lmdeck", name: "lmdeck" }
  ]

  readonly property var providerSettings: {
    var out = {}
    var base = root.settings && root.settings.providers ? root.settings.providers : {}
    for (var id in base) out[id] = base[id]
    for (var pid in providerPrefs) out[pid] = providerPrefs[pid]
    return out
  }

  readonly property var usageSettings: {
    var out = {}
    var src = root.settings || {}
    for (var key in src) out[key] = src[key]
    out.providers = providerSettings
    return out
  }

  // Tab order: the picker's saved order, then the known list (lmdeck last),
  // then anything else in the order it arrived.
  function ordered(items, idOf) {
    var order = root.providerOrder
    var decorated = []
    for (var i = 0; i < items.length; i++) {
      var id = idOf(items[i])
      var rank = order.indexOf(id)
      if (rank < 0) {
        rank = 1000
        for (var k = 0; k < knownProviders.length; k++) if (knownProviders[k].id === id) rank = 500 + k
      }
      decorated.push({ item: items[i], rank: rank, at: i })
    }
    decorated.sort(function(a, b) { return a.rank - b.rank || a.at - b.at })
    return decorated.map(function(d) { return d.item })
  }

  function providerShown(id) {
    var p = providerSettings[id]
    return !p || p.enabled !== false
  }

  // Every provider the picker offers: the known ones, then any other record
  // a collector dropped into the usage directory.
  readonly property var pickerRows: {
    var rev = usage.dataRevision
    var records = {}
    for (var i = 0; i < usage.agents.length; i++) {
      var r = usage.agents[i] ? usage.agents[i].record : null
      if (r && r.id) records[String(r.id)] = r
    }
    var rows = []
    var seen = {}
    function add(id, name) {
      if (seen[id]) return
      seen[id] = true
      var rec = records[id]
      rows.push({
        id: id,
        name: rec && rec.name ? String(rec.name) : name,
        shown: root.providerShown(id),
        // The same test the data layer uses to give an agent a tab.
        hasData: (!!rec && usage.providerHasData(usage.displayProvider(rec))) || (id === "lmdeck" && lmdeck.up)
      })
    }
    for (var k = 0; k < knownProviders.length; k++) add(knownProviders[k].id, knownProviders[k].name)
    for (var extra in records) add(extra, extra)
    return root.ordered(rows, function(row) { return row.id })
  }

  readonly property bool anyProviderAvailable: {
    for (var i = 0; i < pickerRows.length; i++) if (pickerRows[i].hasData) return true
    return false
  }

  function setProviderShown(id, shown) {
    var next = {}
    for (var key in providerPrefs) next[key] = providerPrefs[key]
    next[id] = { enabled: shown }
    providerPrefs = next
    savePrefs()
    if (shown) {
      usage.refreshAll(false)
      collectExtra("normal")
    }
  }

  function moveProvider(id, delta) {
    var ids = pickerRows.map(function(row) { return row.id })
    var from = ids.indexOf(id)
    var to = from + delta
    if (from < 0 || to < 0 || to >= ids.length) return
    ids.splice(to, 0, ids.splice(from, 1)[0])
    providerOrder = ids
    savePrefs()
  }

  // Both maps go out whole, so quick edits land in order instead of racing
  // read-modify-write cycles.
  function savePrefs() {
    Quickshell.execDetached([root.extraTool, "save-prefs",
                             JSON.stringify({ providers: providerPrefs, order: providerOrder })])
  }

  function applyPrefs(text) {
    try {
      var parsed = JSON.parse(String(text || ""))
      providerPrefs = parsed && parsed.providers ? parsed.providers : {}
      providerOrder = parsed && Array.isArray(parsed.order) ? parsed.order : []
    } catch (e) {
      providerPrefs = {}
      providerOrder = []
    }
  }

  FileView {
    id: prefsFile
    path: root.prefsPath
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.applyPrefs(text())
  }

  // Agents Omarchy ships no collector for (Antigravity, Copilot) come from
  // collectors/agents-extra, which writes into the same usage directory.
  property double _extraAt: 0
  function collectExtra(kind) {
    if (extraCollect.running) return
    // Opening the panel asks for limits; one quota call a minute is plenty.
    if (kind === "limits" && Date.now() - _extraAt < 60000) return
    _extraAt = Date.now()
    var command = [root.extraTool, "collect"]
    if (kind === "force") command.push("--force")
    if (kind === "limits") command.push("--limits-only")
    for (var id in providerSettings)
      if (providerSettings[id] && providerSettings[id].enabled === false) command.push("--except", id)
    extraCollect.command = command
    extraCollect.running = true
  }

  Process {
    id: extraCollect
    running: false
    onExited: usage.rescanAgents()
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: if (text.trim() !== "") console.warn("agents-extra", text.trim())
    }
  }

  // Opening resets the scroll and the panel keeps settling for a moment (the
  // lmdeck section, the panel's own height), so `choose` holds the picker in
  // view while the timer runs instead of guessing when layout is done.
  Timer {
    id: pickerScroll
    interval: 800
    onRunningChanged: if (running) root.scrollToPicker()
  }

  function scrollToPicker() {
    if (panelFlick) panelFlick.contentY = Math.max(0, panelFlick.contentHeight - panelFlick.height)
  }

  Connections {
    target: panelFlick
    function onContentHeightChanged() { if (pickerScroll.running) root.scrollToPicker() }
    function onHeightChanged() { if (pickerScroll.running) root.scrollToPicker() }
    function onContentYChanged() { if (pickerScroll.running && root.opened) Qt.callLater(root.scrollToPicker) }
  }

  Timer {
    interval: Math.max(30, Number(usage.refreshIntervalSec) || 900) * 1000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.collectExtra("normal")
  }''')

# The data layer sees the picker's choices: hidden agents drop out of the list
# and their stock collectors are skipped (--except).
sub('''    id: usage
    settings: root.settings''', '''    id: usage
    settings: root.usageSettings''')

sub('''  function refreshNow() {
    usage.refreshAll(true)''', '''  function refreshNow() {
    usage.refreshAll(true)
    collectExtra("force")''')

sub('''    usage.refreshLimits()
    Qt.callLater''', '''    usage.refreshLimits()
    collectExtra("limits")
    Qt.callLater''')

# Hiding every provider must not strand the picker: the icon stays while
# anything could be shown, and the panel opens straight onto the picker.
sub('visible: providers.length > 0\n', 'visible: providers.length > 0 || anyProviderAvailable\n')

sub('''      onTextKey: function(t) { if (t === "r" || t === "R") root.refreshNow() }''',
    '''      onTextKey: function(t) {
        if (t === "r" || t === "R") root.refreshNow()
        else if (t === "p" || t === "P") root.pickerExpanded = !root.pickerExpanded
      }''')

sub('''            text: "No AI coding subscriptions found.\\nAgents show up here once you've used them."''',
    '''            text: root.anyProviderAvailable
              ? "Every provider is hidden.\\nTurn one back on below."
              : "No AI coding subscriptions found.\\nAgents show up here once you've used them."''')

# Picker at the bottom: a header with a Show/Done button, then one switch per provider.
sub('''            horizontalAlignment: Text.AlignHCenter
            elide: Text.ElideRight
          }
        }
      }
    }
  }
''', '''            horizontalAlignment: Text.AlignHCenter
            elide: Text.ElideRight
          }

          // ---------- Provider picker ----------
          PanelSeparator {
            foreground: root.foreground
          }

          Column {
            id: pickerSection
            width: parent.width
            spacing: Style.spacing.sm

            Item {
              width: parent.width
              implicitHeight: Math.max(pickerHeader.implicitHeight, pickerButton.implicitHeight)

              PanelSectionHeader {
                id: pickerHeader
                text: "PROVIDERS"
                foreground: root.foreground
                fontFamily: root.fontFamily
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
              }

              Button {
                id: pickerButton
                visible: root.providers.length > 0
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                text: root.pickerExpanded ? "Done" : "Choose"
                tooltipText: "Pick which providers this panel shows (p)"
                bordered: true
                foreground: root.foreground
                fontFamily: root.fontFamily
                fontSize: Style.font.caption
                verticalPadding: Style.space(2)
                onClicked: root.pickerExpanded = !root.pickerExpanded
              }
            }

            Repeater {
              model: root.pickerOpen ? root.pickerRows : []

              ProviderToggleRow {
                required property var modelData
                required property int index
                width: pickerSection.width
                row: modelData
                first: index === 0
                last: index === root.pickerRows.length - 1
              }
            }
          }
        }
      }
    }
  }

  // One provider in the picker: name, whether it has data, move buttons
  // that set the tab order, and a switch.
  component ProviderToggleRow: Item {
    id: pickRow
    property var row: null
    property bool first: false
    property bool last: false

    implicitHeight: Math.max(pickName.implicitHeight + pickNote.implicitHeight, pickSwitch.implicitHeight) + Style.spacing.sm

    Column {
      anchors.left: parent.left
      anchors.right: pickMove.left
      anchors.rightMargin: Style.spacing.md
      anchors.verticalCenter: parent.verticalCenter

      Text {
        id: pickName
        textFormat: Text.PlainText
        width: parent.width
        text: pickRow.row ? pickRow.row.name : ""
        color: pickRow.row && pickRow.row.shown ? root.foreground : root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        elide: Text.ElideRight
      }

      Text {
        id: pickNote
        textFormat: Text.PlainText
        width: parent.width
        text: !pickRow.row ? "" : (!pickRow.row.shown ? "Hidden" : (pickRow.row.hasData ? "Shown" : "Shown once it has usage"))
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        elide: Text.ElideRight
      }
    }

    Row {
      id: pickMove
      anchors.right: pickSwitch.left
      anchors.rightMargin: Style.spacing.md
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.spacing.xs

      Button {
        text: "\uf077"
        tooltipText: "Move left in the tabs"
        enabled: !pickRow.first
        opacity: enabled ? 1 : 0.3
        foreground: root.foreground
        fontFamily: root.fontFamily
        fontSize: Style.font.caption
        horizontalPadding: Style.space(6)
        verticalPadding: Style.space(2)
        onClicked: if (pickRow.row) root.moveProvider(pickRow.row.id, -1)
      }

      Button {
        text: "\uf078"
        tooltipText: "Move right in the tabs"
        enabled: !pickRow.last
        opacity: enabled ? 1 : 0.3
        foreground: root.foreground
        fontFamily: root.fontFamily
        fontSize: Style.font.caption
        horizontalPadding: Style.space(6)
        verticalPadding: Style.space(2)
        onClicked: if (pickRow.row) root.moveProvider(pickRow.row.id, 1)
      }
    }

    ToggleSwitch {
      id: pickSwitch
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      trackHeight: Math.max(16, Math.round(Style.spacing.controlHeight * 0.42))
      checked: !!pickRow.row && pickRow.row.shown
      foreground: root.foreground
      onToggled: if (pickRow.row) root.setProviderShown(pickRow.row.id, !pickRow.row.shown)
    }
  }
''')

# IPC for scripts: `qs ipc call m1-max.agents showProvider codex`.
sub('''    function next(): string { root.selectProvider(root.providerIndex + 1); return "ok" }''',
    '''    function next(): string { root.selectProvider(root.providerIndex + 1); return "ok" }
    function choose(): void {
      root.pickerExpanded = true
      root.open()
      pickerScroll.restart()
    }
    function showProvider(id: string): string { root.setProviderShown(id, true); return "ok" }
    function hideProvider(id: string): string { root.setProviderShown(id, false); return "ok" }
    function moveProvider(id: string, delta: int): string { root.moveProvider(id, delta); return "ok" }
    function listProviders(): string {
      var out = []
      for (var i = 0; i < root.pickerRows.length; i++)
        out.push(root.pickerRows[i].id + (root.pickerRows[i].shown ? " shown" : " hidden"))
      return out.join("\\n")
    }''')

# Hero line for lmdeck: live state instead of a plan name.
sub('''  function heroMeta(p) {
    if (!p) return ""''', '''  function heroMeta(p) {
    if (!p) return ""
    if (p.providerId === "lmdeck") {
      if (!lmdeck.up) return "Server stopped"
      var n = lmdeck.running.length
      return n === 0 ? "Local models · idle" : (lmdeck.busy ? "Generating · " : "Loaded · ") + lmdeck.running[0].name.split("/").pop()
    }''')

# Marks come from the stock assets directory; lmdeck, Antigravity and Copilot ship their own.
sub('''    if (!p) return []
    var candidates = []''', '''    if (!p) return []
    if (p.providerId === "lmdeck") return [Qt.resolvedUrl("assets/lmdeck.svg")]
    var markDir = root.forkMarks.indexOf(p.providerId) >= 0
      ? String(Qt.resolvedUrl("assets/"))
      : "file://''' + str(STOCK) + '''/assets/"
    var candidates = []''')
sub('Qt.resolvedUrl("assets/" + p.providerId + "-light.svg")', 'markDir + p.providerId + "-light.svg"')
sub('Qt.resolvedUrl("assets/" + p.providerId + ".svg")', 'markDir + p.providerId + ".svg"')

# Open on lmdeck while it is running; tell the client when to poll.
sub('''  onOpenedChanged: if (opened) {
    cursorActive = false''', '''  property bool _pickedThisSession: false
  onSelectedProviderIdChanged: if (opened) _pickedThisSession = true
  Connections {
    target: root
    function onOpenedChanged() {
      if (root.opened && lmdeck.up && !root._pickedThisSession) root.selectedProviderId = "lmdeck"
    }
  }
  onOpenedChanged: if (opened) {
    cursorActive = false''')

sub('''  Main {
    id: usage''', '''  Lmdeck {
    id: lmdeck
    baseUrl: String((root.settings && root.settings.lmdeckUrl) || "http://127.0.0.1:11434")
    panelOpen: root.opened
  }

  Stock.Main {
    id: usage''')

# Bar icon: a small dot while a model is loaded (accent while busy).
sub('''    text: "󱚣"
    active: root.alarming''', '''    text: "󱚣"
    active: root.alarming

    Rectangle {
      visible: lmdeck.up && (lmdeck.running.length > 0 || lmdeck.busy)
      width: Math.max(4, Math.round(parent.height * 0.16))
      height: width
      radius: width / 2
      color: lmdeck.busy ? Color.accent : root.foreground
      anchors.right: parent.right
      anchors.top: parent.top
      anchors.rightMargin: Math.round(parent.width * 0.18)
      anchors.topMargin: Math.round(parent.height * 0.18)
    }''')


# The search box needs plain typing while focused.
sub('''      id: keyCatcher
      anchors.fill: parent
''', '''      id: keyCatcher
      anchors.fill: parent
      blocked: lmdeckSection.searchFocused
''')

# Live section right under the provider switch.
sub('''          // ---------- Status ----------''', '''          LmdeckSection {
            id: lmdeckSection
            visible: root.lmdeckSelected
            width: parent.width
            lm: lmdeck
            foreground: root.foreground
            dim: root.dim
            track: root.track
            urgent: root.urgent
            fontFamily: root.fontFamily
          }

          // ---------- Status ----------''')

header = "// GENERATED by sync-from-stock.py from " + str(STOCK / "Panel.qml") + " — edit the script, not this file.\n"
(HERE / "Panel.qml").write_text(header + s)
print("Panel.qml regenerated")
