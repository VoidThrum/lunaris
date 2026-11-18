import QtQuick
import QtQuick.Controls
import "Cards.js" as Cards

// The ways to connect to a card's computer, as small chips in a row that
// wraps: a dot (can it be connected to now), the type's logo when it has one
// and a short name ("Lunaris", "RustDesk", ...). One is chosen; a click
// chooses another.
// `methods`: [{ key, label, online, host | source }] (Cards.js). The names can come from
// another computer: plain text only.
Flow {
    id: chips
    property var methods: []
    property string current: ""
    // On a coloured background (a card's top): white on a dark veil.
    property bool onColor: false
    // All of them in one row: the widest are cut short ("Sunshine / Ga…")
    // until they fit. Otherwise the row wraps.
    property bool oneRow: false
    // The dot that says whether a way can be connected to now. Left out
    // where that is not known (the app's own computers: all can be tried).
    property bool dots: true
    signal chosen(string key)
    spacing: 5

    FontMetrics { id: chipMetrics; font.pixelSize: 10; font.weight: Font.DemiBold }
    // What a chip has besides its name: the dot and the margins, and its
    // type's logo when it has one.
    readonly property int chipExtra: dots ? 25 : 14
    readonly property int logoExtra: 18
    function extraOf(method) { return chipExtra + (Cards.typeLogo(Cards.methodType(method)) !== "" ? logoExtra : 0); }
    // The widest a chip may be.
    readonly property real chipCap: {
        var room = chips.width > 0 ? chips.width : 400;
        if (!chips.oneRow || chips.methods.length === 0) return room;
        var natural = chips.methods.map(function (m) { return Math.ceil(chipMetrics.advanceWidth(String(m.label))) + chips.extraOf(m); });
        room -= chips.spacing * (natural.length - 1);
        var cap = Math.max.apply(null, natural);
        function total(c) { return natural.reduce(function (sum, w) { return sum + Math.min(w, c); }, 0); }
        while (cap > 30 && total(cap) > room) cap -= 1;
        return cap;
    }
    Accessible.role: Accessible.Grouping
    Accessible.name: I18n.t("Ways to connect")

    Repeater {
        model: chips.methods
        delegate: AbstractButton {
            id: chip
            required property var modelData
            readonly property bool isCurrent: modelData.key === chips.current
            objectName: "methodChip"
            property string methodKey: modelData.key
            implicitHeight: 20
            implicitWidth: Math.min(chips.chipCap, Math.ceil(chipMetrics.advanceWidth(String(modelData.label))) + chips.extraOf(modelData))
            hoverEnabled: true
            activeFocusOnTab: true
            Accessible.role: Accessible.RadioButton
            Accessible.name: modelData.label + ", " + I18n.t(modelData.online ? "Online" : "Offline")
            Accessible.checked: isCurrent
            Keys.onReturnPressed: chip.clicked()
            Keys.onSpacePressed: chip.clicked()
            onClicked: chips.chosen(modelData.key)
            ToolTip.visible: hovered
            ToolTip.delay: 500
            // A tool tip takes rich text: a name from another computer is
            // shown without the character that would start a tag.
            ToolTip.text: String(modelData.label).replace(/</g, "\u2039") + " · " + I18n.t(modelData.online ? "Online" : "Offline")
            background: Rectangle {
                radius: 10
                color: chips.onColor ? (chip.isCurrent ? Qt.rgba(0, 0, 0, 0.55) : Qt.rgba(0, 0, 0, 0.28))
                                     : (chip.isCurrent ? Theme.accentA(0.16) : (chip.hovered ? Theme.surface2 : "transparent"))
                border.color: chips.onColor ? (chip.isCurrent ? "#ffffff" : Qt.rgba(1, 1, 1, 0.3))
                                            : (chip.isCurrent ? Theme.accent : (chip.activeFocus ? Theme.accentFg : Theme.borderStrong))
                border.width: 1
            }
            contentItem: Item {
                Rectangle {
                    id: chipDot
                    visible: chips.dots
                    width: chips.dots ? 6 : 0; height: 6; radius: 3
                    anchors.left: parent.left
                    anchors.leftMargin: chips.dots ? 7 : 2
                    anchors.verticalCenter: parent.verticalCenter
                    color: chip.modelData.online ? "#22c55e" : (chips.onColor ? Qt.rgba(1, 1, 1, 0.5) : Theme.textSubtle)
                }
                TypeLogo {
                    id: chipLogo
                    objectName: "chipLogo"
                    anchors.left: chipDot.right
                    anchors.leftMargin: shown ? 5 : 0
                    anchors.verticalCenter: parent.verticalCenter
                    typeId: Cards.methodType(chip.modelData)
                    size: 12
                    tile: false
                    letter: false
                }
                Text {
                    id: chipLabel
                    anchors.left: chipLogo.right
                    anchors.leftMargin: 5
                    anchors.right: parent.right
                    anchors.rightMargin: 7
                    anchors.verticalCenter: parent.verticalCenter
                    textFormat: Text.PlainText
                    text: chip.modelData.label
                    color: chips.onColor ? "#ffffff" : (chip.isCurrent ? Theme.accentFg : Theme.text)
                    font.pixelSize: 10
                    font.weight: chip.isCurrent ? Font.DemiBold : Font.Medium
                    elide: Text.ElideRight
                }
            }
        }
    }
}
