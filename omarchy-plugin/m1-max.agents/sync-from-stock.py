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

# Providers: stock records, with lmdeck last (far right) (synthetic tab while no record exists yet).
sub('readonly property var providers: usage.enabledProviders',
    '''readonly property var providers: {
    var list = (usage.enabledProviders || []).slice()
    var at = -1
    for (var i = 0; i < list.length; i++) if (list[i].providerId === "lmdeck") at = i
    var rec = at >= 0 ? list.splice(at, 1)[0] : null
    if (rec || lmdeck.up) {
      var p = rec || { providerId: "lmdeck", providerName: "lmdeck", limits: [], recentDays: [], modelUsage: ({}),
                       tierLabel: "", usageStatusText: "", authHelpText: "", hasPromptStats: true }
      list.push(p)
    }
    return list
  }
  readonly property bool lmdeckSelected: !!provider && provider.providerId === "lmdeck"''')

# Hero line for lmdeck: live state instead of a plan name.
sub('''  function heroMeta(p) {
    if (!p) return ""''', '''  function heroMeta(p) {
    if (!p) return ""
    if (p.providerId === "lmdeck") {
      if (!lmdeck.up) return "Server stopped"
      var n = lmdeck.running.length
      return n === 0 ? "Local models · idle" : (lmdeck.busy ? "Generating · " : "Loaded · ") + lmdeck.running[0].name.split("/").pop()
    }''')

# Marks come from the stock assets directory (lmdeck ships its own).
sub('''    if (!p) return []
    var candidates = []''', '''    if (!p) return []
    if (p.providerId === "lmdeck") return [Qt.resolvedUrl("assets/lmdeck.svg")]
    var candidates = []''')
sub('Qt.resolvedUrl("assets/" + p.providerId + "-light.svg")', '"file://' + str(STOCK) + '/assets/" + p.providerId + "-light.svg"')
sub('Qt.resolvedUrl("assets/" + p.providerId + ".svg")', '"file://' + str(STOCK) + '/assets/" + p.providerId + ".svg"')

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

sub('contentWidth: panel.fittedContentWidth(Style.space(380))',
    'contentWidth: panel.fittedContentWidth(Style.space(root.lmdeckSelected ? 440 : 380))')

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

sub('''            text: "No AI coding subscriptions found.\\nAgents show up here once you've used them."''',
    '''            text: "No AI coding subscriptions found.\\nAgents show up here once you've used them."''')

header = "// GENERATED by sync-from-stock.py from " + str(STOCK / "Panel.qml") + " — edit the script, not this file.\n"
(HERE / "Panel.qml").write_text(header + s)
print("Panel.qml regenerated")
