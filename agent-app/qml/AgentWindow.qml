import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Qt.labs.platform as Platform
import com.lunaris.agent 1.0

ApplicationWindow {
    id: window
    // Wide rather than tall: two columns of cards side by side, one column
    // when the window is made narrow; what does not fit scrolls.
    width: 900
    height: 660
    minimumWidth: 420
    minimumHeight: 460
    visible: false
    title: "Lunaris Agent"
    color: Theme.bg
    flags: Qt.Window

    // Room for the activity log while it is open.
    onShowLogChanged: { if (showLog && window.height < 820) window.height = 820 }

    // ── Bridge ──
    AgentBridge { id: bridge }

    // Hidden helper for clipboard access
    TextField { id: clipHelper; visible: false }

    // ── State ──
    property bool agentActive: false
    property bool agentConnected: false
    property string agentId: ""
    property string agentName: ""
    // The updater's state (bridge.updateStatus(), see updater/src/service.rs).
    property var update: ({ state: "idle", current: "", latest: "", notes: "", releaseUrl: "",
                            canInstall: false, manualMessage: "", progress: 0, error: "",
                            errorKind: "", autoCheck: true })
    property bool updateDismissed: false
    readonly property bool updateBusy: ["downloading", "verifying", "installing", "restart"].indexOf(update.state) >= 0
    property bool closeToTray: false
    property bool showLog: false
    property string notice: ""
    property bool noticeError: false
    // Direct connections (clients connect to this computer, no server)
    property bool directEnabled: false
    property int directPort: 8091
    property bool rustdeskEnabled: false
    property int rustdeskPort: 21118
    property bool rustdeskFileTransfer: false
    // Screen capture on a Linux Wayland session: "auto" or "portal".
    property string waylandCapture: "auto"
    property string directPassword: ""
    property var directAddresses: []

    readonly property string statusText: agentConnected ? "Online"
        : (agentActive ? (directEnabled ? "Ready for direct connections" : "Connecting…") : "Offline")
    readonly property color statusColor: (agentConnected || (agentActive && directEnabled)) ? Theme.success
        : (agentActive ? Theme.warning : Theme.textSubtle)

    // ── Tray ──
    Platform.SystemTrayIcon {
        id: trayIcon
        visible: true
        icon.source: "qrc:/icons/icon.png"
        tooltip: "Lunaris Agent — " + window.statusText
        menu: Platform.Menu {
            Platform.MenuItem { text: window.visible ? "Hide" : "Show"; onTriggered: toggleWindow() }
            Platform.MenuItem { text: agentActive ? "Stop sharing" : "Start sharing"; onTriggered: agentActive ? bridge.stopAgent() : saveAndStart() }
            Platform.MenuSeparator {}
            Platform.MenuItem { text: "Quit"; onTriggered: Qt.quit() }
        }
        onActivated: (reason) => { if (reason === Platform.SystemTrayIcon.Trigger) toggleWindow() }
    }
    function toggleWindow() { if (window.visible) { window.hide() } else { window.show(); window.raise() } }
    function saveForm() {
        bridge.saveConfig(fieldServerUrl.text, fieldServerToken.text, cbAutostart.checked, cbCloseToTray.checked)
    }
    // Start always uses what is in the form: persist it first, otherwise the
    // agent connects with the previously saved server URL / token.
    function saveAndStart() { saveForm(); bridge.startAgent() }
    // Direct-connection settings apply when sharing (re)starts.
    function restartIfSharing() { if (window.agentActive) { bridge.stopAgent(); restartTimer.restart() } }
    function copyText(t) { clipHelper.text = t; clipHelper.selectAll(); clipHelper.copy(); clipHelper.text = "" }
    function flash(msg, isError) { window.notice = msg; window.noticeError = isError; noticeTimer.restart() }
    function pollUpdate() {
        var s
        try { s = JSON.parse(bridge.updateStatus()) } catch (e) { return }
        window.update = s
        // Installed: quit, the agent starts again by itself.
        if (s.state === "restart" && bridge.updateRestart()) Qt.quit()
    }
    function updateStateText() {
        var u = window.update
        switch (u.state) {
        case "checking": return "Checking for updates…"
        case "upToDate": return "Lunaris is up to date"
        case "downloading": return "Downloading " + Math.round(u.progress * 100) + "%…"
        case "verifying": return "Checking the signature…"
        case "installing": return "Installing…"
        case "restart": return "Restarting…"
        case "error": return u.error
        case "available": return "Lunaris " + u.latest + " is available"
        }
        return ""
    }

    // ── Timers ──
    Timer { interval: 100;  running: true; repeat: true; onTriggered: bridge.pollLogs() }
    Timer { interval: 2000; running: true; repeat: true; onTriggered: bridge.pollStatus() }
    // The updater checks by itself (at start, then daily); this shows its state.
    Timer { interval: 1000; running: true; repeat: true; triggeredOnStart: true; onTriggered: pollUpdate() }
    Timer { id: noticeTimer; interval: 3500; onTriggered: window.notice = "" }
    // Give the stopped agent a moment to release the direct-connection port.
    Timer { id: restartTimer; interval: 800; onTriggered: bridge.startAgent() }

    // ── Reusable pieces ──
    component Card: Rectangle {
        color: Theme.surface
        border.color: Theme.border
        border.width: 1
        radius: Theme.radiusLg
    }
    component SectionLabel: Text {
        color: Theme.textMuted
        font.pixelSize: 11
        font.weight: Font.DemiBold
        font.letterSpacing: 0.4
    }
    component Field: TextField {
        Layout.fillWidth: true
        implicitHeight: 34
        leftPadding: 10; rightPadding: 10
        font.pixelSize: 13
        color: Theme.textStrong
        placeholderTextColor: Theme.textSubtle
        selectByMouse: true
        background: Rectangle {
            radius: Theme.radiusSm
            color: Theme.surface2
            border.width: 1
            border.color: parent.activeFocus ? Theme.accentFg : Theme.borderStrong
        }
    }
    component Toggle: CheckBox {
        id: toggle
        spacing: 8
        indicator: Rectangle {
            implicitWidth: 18; implicitHeight: 18
            x: toggle.leftPadding; y: (toggle.height - height) / 2
            radius: 4
            color: toggle.checked ? Theme.accent : Theme.surface2
            border.color: toggle.checked ? Theme.accent : Theme.borderStrong
            Text {
                anchors.centerIn: parent
                visible: toggle.checked
                text: "✓"; color: Theme.textOnAccent
                font.pixelSize: 12; font.bold: true
            }
        }
        contentItem: Text {
            leftPadding: toggle.indicator.width + toggle.spacing
            text: toggle.text
            color: Theme.text
            font.pixelSize: 13
            verticalAlignment: Text.AlignVCenter
        }
    }
    component GhostButton: Rectangle {
        id: ghost
        property alias label: ghostLabel.text
        property bool danger: false
        signal clicked()
        implicitHeight: 38
        implicitWidth: ghostLabel.implicitWidth + 28
        radius: Theme.radius
        color: ghostMa.containsMouse ? Theme.surface2 : Theme.surface
        border.width: 1
        border.color: danger ? Theme.danger : Theme.borderStrong
        Text {
            id: ghostLabel
            anchors.centerIn: parent
            font.pixelSize: 13; font.weight: Font.DemiBold
            color: ghost.danger ? Theme.danger : Theme.textStrong
        }
        MouseArea { id: ghostMa; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: ghost.clicked() }
    }
    component LinkButton: Text {
        id: link
        signal clicked()
        color: linkMa.containsMouse ? Theme.accentFg : Theme.textMuted
        font.pixelSize: 12
        font.underline: linkMa.containsMouse
        MouseArea { id: linkMa; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: link.clicked() }
    }

    // ── Layout ──
    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 20
        spacing: 14

        // Header: brand + status pill
        RowLayout {
            Layout.fillWidth: true
            spacing: 10
            Image { source: "qrc:/icons/icon.png"; sourceSize: Qt.size(28, 28); Layout.preferredWidth: 28; Layout.preferredHeight: 28 }
            Text { text: "Lunaris Agent"; color: Theme.textStrong; font.pixelSize: 17; font.weight: Font.DemiBold; Layout.fillWidth: true }
            Rectangle {
                implicitHeight: 26
                implicitWidth: statusRow.implicitWidth + 20
                radius: 13
                color: Theme.surface2
                border.color: Theme.border
                RowLayout {
                    id: statusRow
                    anchors.centerIn: parent
                    spacing: 6
                    Rectangle { width: 8; height: 8; radius: 4; color: window.statusColor }
                    Text { text: window.statusText; color: Theme.text; font.pixelSize: 12; font.weight: Font.Medium }
                }
            }
        }

        // Update banner: a newer release, the download/install progress, or
        // why the update failed.
        Rectangle {
            id: updateBanner
            objectName: "updateBanner"
            visible: window.update.latest !== "" && !window.updateDismissed
                     && (window.update.state === "available" || window.updateBusy
                         || (window.update.state === "error" && window.update.errorKind !== "check"))
            Layout.fillWidth: true
            implicitHeight: bannerCol.implicitHeight + 16
            radius: Theme.radius
            color: window.update.state === "error" ? Qt.rgba(Theme.danger.r, Theme.danger.g, Theme.danger.b, 0.10) : Theme.accentA(0.12)
            border.color: window.update.state === "error" ? Theme.danger : Theme.accentA(0.4)
            ColumnLayout {
                id: bannerCol
                anchors.left: parent.left; anchors.right: parent.right; anchors.verticalCenter: parent.verticalCenter
                anchors.leftMargin: 12; anchors.rightMargin: 8
                spacing: 4
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 10
                    Text {
                        objectName: "updateBannerText"
                        text: window.update.state === "available" ? "Lunaris " + window.update.latest + " is available" : updateStateText()
                        color: window.update.state === "error" ? Theme.danger : Theme.textStrong
                        font.pixelSize: 12; Layout.fillWidth: true; wrapMode: Text.Wrap
                    }
                    LinkButton {
                        visible: !window.updateBusy && window.update.canInstall
                        objectName: "updateBannerInstall"
                        text: window.update.state === "error" ? "Try again" : "Update"
                        color: Theme.accentFg; font.weight: Font.DemiBold
                        onClicked: bridge.updateInstall()
                    }
                    LinkButton {
                        visible: !window.updateBusy && !window.update.canInstall
                        text: "Download"; color: Theme.accentFg
                        onClicked: bridge.openUrl(window.update.releaseUrl)
                    }
                    LinkButton { visible: !window.updateBusy; text: "✕"; onClicked: window.updateDismissed = true }
                }
                Text {
                    Layout.fillWidth: true
                    visible: window.update.state === "available" && !window.update.canInstall && window.update.manualMessage !== ""
                    text: window.update.manualMessage
                    color: Theme.textMuted; font.pixelSize: 11; wrapMode: Text.Wrap
                }
                ProgressBar {
                    Layout.fillWidth: true
                    visible: window.update.state === "downloading"
                    from: 0; to: 1; value: window.update.progress
                }
            }
        }

        ScrollView {
            id: scroll
            Layout.fillWidth: true
            Layout.fillHeight: true
            contentWidth: availableWidth
            clip: true

        GridLayout {
            width: scroll.availableWidth
            columns: width >= 760 ? 2 : 1
            columnSpacing: 14
            rowSpacing: 14

        ColumnLayout {
            Layout.fillWidth: true
            Layout.preferredWidth: 1
            Layout.alignment: Qt.AlignTop
            spacing: 14

        // This computer
        Card {
            Layout.fillWidth: true
            implicitHeight: thisPc.implicitHeight + 28
            ColumnLayout {
                id: thisPc
                anchors.fill: parent; anchors.margins: 14
                spacing: 6
                SectionLabel { text: "THIS COMPUTER" }
                Text {
                    text: window.agentName !== "" ? window.agentName : "—"
                    color: Theme.textStrong; font.pixelSize: 15; font.weight: Font.DemiBold
                    elide: Text.ElideRight; Layout.fillWidth: true
                }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 8
                    Text {
                        text: window.agentId !== "" ? "ID " + window.agentId : "ID is assigned on first start"
                        color: Theme.textMuted; font.pixelSize: 12; font.family: "monospace"
                        elide: Text.ElideMiddle; Layout.fillWidth: true
                    }
                    LinkButton {
                        visible: window.agentId !== ""
                        text: "Copy"
                        onClicked: { copyText(window.agentId); flash("ID copied", false) }
                    }
                }
            }
        }

        // Server connection
        Card {
            Layout.fillWidth: true
            implicitHeight: serverCol.implicitHeight + 28
            ColumnLayout {
                id: serverCol
                anchors.fill: parent; anchors.margins: 14
                spacing: 8
                RowLayout {
                    Layout.fillWidth: true
                    SectionLabel { text: "SERVER"; Layout.fillWidth: true }
                    LinkButton { text: "Import config…"; onClicked: { bridge.importConfig(); bridge.loadConfig() } }
                }
                Text { text: "Address"; color: Theme.text; font.pixelSize: 12 }
                Field { id: fieldServerUrl; placeholderText: "ws://your-server:8090" }
                Text { text: "Agent token"; color: Theme.text; font.pixelSize: 12; Layout.topMargin: 2 }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    Field { id: fieldServerToken; echoMode: showToken.checked ? TextInput.Normal : TextInput.Password; placeholderText: "From the server's web UI or server_token.txt" }
                    LinkButton {
                        id: showToken
                        property bool checked: false
                        text: checked ? "Hide" : "Show"
                        onClicked: checked = !checked
                    }
                }
            }
        }

        // Options
        ColumnLayout {
            Layout.fillWidth: true
            spacing: 2
            Toggle { id: cbAutostart; text: "Start when I sign in" }
            Toggle { id: cbCloseToTray; text: "Keep running in the tray when closed"; onCheckedChanged: window.closeToTray = checked }
        }
        }

        ColumnLayout {
            Layout.fillWidth: true
            Layout.preferredWidth: 1
            Layout.alignment: Qt.AlignTop
            spacing: 14

        // Direct connections
        Card {
            Layout.fillWidth: true
            implicitHeight: directCol.implicitHeight + 28
            ColumnLayout {
                id: directCol
                anchors.fill: parent; anchors.margins: 14
                spacing: 8
                SectionLabel { text: "DIRECT CONNECTION" }
                Toggle {
                    id: cbDirect
                    text: "Allow direct connections (no server needed)"
                    checked: window.directEnabled
                    onToggled: { bridge.setDirectEnabled(checked); restartIfSharing() }
                }
                Text {
                    visible: !window.directEnabled
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    text: "Clients on your network or VPN connect with this computer's address and a password."
                    color: Theme.textMuted; font.pixelSize: 12
                }
                ColumnLayout {
                    visible: window.directEnabled
                    Layout.fillWidth: true
                    spacing: 4
                    Text { text: "Addresses (click one to copy)"; color: Theme.text; font.pixelSize: 12 }
                    // Click an address to copy it.
                    Flow {
                        Layout.fillWidth: true
                        spacing: 6
                        Repeater {
                            model: window.directAddresses
                            Rectangle {
                                implicitHeight: 26
                                implicitWidth: addrText.implicitWidth + 18
                                radius: Theme.radiusSm
                                color: addrMa.containsMouse ? Theme.surface2 : "transparent"
                                border.width: 1
                                border.color: Theme.borderStrong
                                Text {
                                    id: addrText
                                    anchors.centerIn: parent
                                    text: modelData + ":" + window.directPort
                                    color: Theme.textStrong; font.pixelSize: 12; font.family: "monospace"
                                }
                                MouseArea {
                                    id: addrMa
                                    anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor
                                    onClicked: { copyText(modelData + ":" + window.directPort); flash("Address copied", false) }
                                    ToolTip.visible: containsMouse; ToolTip.delay: 400; ToolTip.text: "Copy"
                                }
                            }
                        }
                    }
                    Text {
                        visible: window.directAddresses.length === 0
                        text: "No network address found"
                        color: Theme.textMuted; font.pixelSize: 12
                    }
                    Text { text: "Password"; color: Theme.text; font.pixelSize: 12; Layout.topMargin: 4 }
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 8
                        Text {
                            text: showDirectPassword.checked ? window.directPassword : "••••-••••-••••"
                            color: Theme.textStrong; font.pixelSize: 13; font.family: "monospace"
                            Layout.fillWidth: true
                        }
                        LinkButton {
                            id: showDirectPassword
                            property bool checked: false
                            text: checked ? "Hide" : "Show"
                            onClicked: checked = !checked
                        }
                        LinkButton { text: "Copy"; onClicked: { copyText(window.directPassword); flash("Password copied", false) } }
                        LinkButton {
                            text: "New"
                            onClicked: { bridge.regenerateDirectPassword(); restartIfSharing(); flash("New password set", false) }
                        }
                    }
                }
            }
        }

        // RustDesk clients
        Card {
            Layout.fillWidth: true
            implicitHeight: rustdeskCol.implicitHeight + 28
            ColumnLayout {
                id: rustdeskCol
                anchors.fill: parent; anchors.margins: 14
                spacing: 8
                SectionLabel { text: "RUSTDESK CLIENTS" }
                Toggle {
                    text: "Allow RustDesk clients (LAN, direct IP)"
                    checked: window.rustdeskEnabled
                    onToggled: { bridge.setRustdesk(checked, window.rustdeskPort); restartIfSharing() }
                }
                Toggle {
                    text: "Let RustDesk clients use their file manager here"
                    enabled: window.rustdeskEnabled
                    checked: window.rustdeskFileTransfer
                    onToggled: { bridge.setRustdeskFileTransfer(checked); restartIfSharing() }
                }
                Text {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    text: "They can then read and change files in this computer's home folder (or what file transfer may reach here)."
                    color: Theme.textMuted; font.pixelSize: 12
                }
                Text {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    text: window.rustdeskEnabled
                        ? "In RustDesk, connect to this computer's address and port " + window.rustdeskPort + " with the access password. Not encrypted: for your own network or a VPN only."
                        : "Lets the RustDesk app connect by this computer's address with the access password. Not encrypted: for your own network or a VPN only."
                    color: Theme.textMuted; font.pixelSize: 12
                }
            }
        }

        // Updates
        Card {
            Layout.fillWidth: true
            implicitHeight: updatesCol.implicitHeight + 28
            ColumnLayout {
                id: updatesCol
                anchors.fill: parent; anchors.margins: 14
                spacing: 8
                SectionLabel { text: "UPDATES" }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 8
                    Text {
                        text: "Version " + window.update.current
                        color: Theme.text; font.pixelSize: 13; Layout.fillWidth: true
                    }
                    LinkButton {
                        objectName: "updateCheckNow"
                        visible: !window.updateBusy && window.update.state !== "checking"
                        text: "Check now"
                        onClicked: bridge.checkForUpdates()
                    }
                }
                Toggle {
                    text: "Check for updates automatically"
                    checked: window.update.autoCheck
                    onToggled: bridge.setUpdateAutoCheck(checked)
                }
                Text {
                    Layout.fillWidth: true
                    visible: text !== "" && window.update.state !== "available"
                    text: updateStateText()
                    color: window.update.state === "error" ? Theme.danger : Theme.textMuted
                    font.pixelSize: 12; wrapMode: Text.Wrap
                }
                Text {
                    Layout.fillWidth: true
                    visible: window.update.latest !== "" && window.update.notes !== ""
                    text: window.update.notes
                    textFormat: Text.PlainText
                    color: Theme.textMuted; font.pixelSize: 11; wrapMode: Text.Wrap
                    maximumLineCount: 8; elide: Text.ElideRight
                }
            }
        }

        // Screen capture on Wayland (Linux only)
        Card {
            visible: Qt.platform.os === "linux"
            Layout.fillWidth: true
            implicitHeight: waylandCol.implicitHeight + 28
            ColumnLayout {
                id: waylandCol
                anchors.fill: parent; anchors.margins: 14
                spacing: 8
                SectionLabel { text: "WAYLAND SCREEN CAPTURE" }
                Toggle {
                    text: "Ask through the portal"
                    checked: window.waylandCapture === "portal"
                    onToggled: { bridge.setWaylandCapture(checked ? "portal" : "auto"); restartIfSharing() }
                }
                Text {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    text: window.waylandCapture === "portal"
                        ? "The capture service is never used: the person at this computer approves on the desktop (remembered until withdrawn there), which shows the screen is shared."
                        : "Automatic: no prompt when the capture service is installed, otherwise the desktop asks. For Linux desktops on Wayland."
                    color: Theme.textMuted; font.pixelSize: 12
                }
            }
        }

        }
        }
        }

        // Actions
        RowLayout {
            Layout.fillWidth: true
            spacing: 8
            Rectangle {
                id: primary
                Layout.fillWidth: true
                implicitHeight: 38
                radius: Theme.radius
                visible: !window.agentActive
                color: primaryMa.containsMouse ? Theme.accentHover : Theme.accent
                Text { anchors.centerIn: parent; text: "Start sharing"; color: Theme.textOnAccent; font.pixelSize: 13; font.weight: Font.DemiBold }
                MouseArea { id: primaryMa; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: saveAndStart() }
            }
            GhostButton {
                Layout.fillWidth: true
                visible: window.agentActive
                danger: true
                label: "Stop sharing"
                onClicked: bridge.stopAgent()
            }
            GhostButton { label: "Save"; onClicked: saveForm() }
        }

        Text {
            Layout.fillWidth: true
            visible: window.notice !== ""
            text: window.notice
            color: window.noticeError ? Theme.danger : Theme.textMuted
            font.pixelSize: 12
            wrapMode: Text.Wrap
        }

        // Activity log
        RowLayout {
            Layout.fillWidth: true
            spacing: 12
            LinkButton {
                text: (window.showLog ? "▾ " : "▸ ") + "Activity log (" + consoleModel.count + ")"
                color: Theme.textMuted
                onClicked: window.showLog = !window.showLog
            }
            Item { Layout.fillWidth: true }
            LinkButton {
                visible: window.showLog
                text: "Copy"
                onClicked: {
                    var lines = []; for (var i = 0; i < consoleModel.count; i++) lines.push(consoleModel.get(i).text)
                    copyText(lines.join("\n")); flash("Log copied", false)
                }
            }
            LinkButton { visible: window.showLog; text: "Clear"; onClicked: { consoleModel.clear(); bridge.clearLogs() } }
        }

        Rectangle {
            visible: window.showLog
            Layout.fillWidth: true; Layout.preferredHeight: 200; Layout.minimumHeight: 140
            color: Theme.surface2; border.color: Theme.border; border.width: 1; radius: Theme.radius
            ListView {
                id: consoleView
                anchors.fill: parent; anchors.margins: 6
                clip: true; cacheBuffer: 500
                model: ListModel { id: consoleModel }
                delegate: Text {
                    text: model.text; color: model.colorStr
                    font.family: "monospace"; font.pixelSize: 11; lineHeight: 1.25
                    leftPadding: 4; width: consoleView.width - 12
                    wrapMode: Text.WrapAnywhere
                }
                ScrollBar.vertical: ScrollBar {}
                onCountChanged: { if (count > 0) positionViewAtEnd() }
            }
        }

    }

    // ── Signals ──
    Connections {
        target: bridge
        function onStatusChanged(active, connected) { window.agentActive = active; window.agentConnected = connected }
        function onLogMessage(msg) {
            var m = msg.toString ? msg.toString() : String(msg)
            var s = m.toLowerCase()
            var clr = Theme.textMuted
            if (s.indexOf("error") >= 0 || s.indexOf("fail") >= 0) clr = Theme.danger
            else if (s.indexOf("warn") >= 0) clr = Theme.warning
            else if (s.indexOf("info") >= 0) clr = Theme.text
            consoleModel.append({ text: m, colorStr: String(clr) })
            if (consoleModel.count > 500) consoleModel.remove(0)
        }
        function onConfigLoaded(srv, tok, name, id, autostart, closetray) {
            fieldServerUrl.text = srv; fieldServerToken.text = tok
            window.agentName = name; window.agentId = id
            cbAutostart.checked = autostart; cbCloseToTray.checked = closetray
            window.closeToTray = closetray
        }
        function onConfigSaved(success, error) {
            if (success) flash("Settings saved", false); else flash("Could not save settings: " + error, true)
        }
        function onImportCompleted(success, error) {
            if (success) flash("Configuration imported", false); else if (error !== "") flash("Import failed: " + error, true)
        }
        function onRustdeskConfigLoaded(enabled, port) {
            window.rustdeskEnabled = enabled; window.rustdeskPort = port
        }
        function onRustdeskFileTransferLoaded(enabled) { window.rustdeskFileTransfer = enabled }
        function onWaylandCaptureLoaded(mode) { window.waylandCapture = mode.toString() }
        function onDirectConfigLoaded(enabled, port, password, addresses) {
            window.directEnabled = enabled; window.directPort = port; window.directPassword = password
            var a = addresses.toString(); window.directAddresses = a === "" ? [] : a.split("\n")
        }
    }

    onClosing: (close) => { if (window.closeToTray) { close.accepted = false; window.hide() } }
    Component.onCompleted: {
        bridge.loadConfig(); bridge.loadDirectConfig(); window.show()
        // Started again after an update while it was sharing: share again.
        if (bridge.resumeSharingAfterUpdate()) saveAndStart()
    }
}
