import QtQuick
import QtQuick.Controls

// One type of computer in the row of types of "Add computer": its logo and,
// under it, its short name ("RustDesk"); the whole name is its tool tip and
// what a screen reader says. The chosen one has the accent's border.
// The names can come from another computer: plain text only.
AbstractButton {
    id: tile
    // `text` is the short name.
    property string typeId: ""
    property string fullName: ""
    property bool chosen: false
    implicitWidth: 96
    implicitHeight: 70
    hoverEnabled: true
    activeFocusOnTab: true
    Accessible.role: Accessible.RadioButton
    Accessible.name: tile.fullName !== "" ? tile.fullName : tile.text
    Accessible.checked: tile.chosen
    Keys.onReturnPressed: tile.clicked()
    Keys.onEnterPressed: tile.clicked()
    Keys.onSpacePressed: tile.clicked()
    ToolTip.visible: hovered && tile.fullName !== "" && tile.fullName !== tile.text
    ToolTip.delay: 500
    // A tool tip takes rich text: a name from another computer is shown
    // without the character that would start a tag.
    ToolTip.text: tile.fullName.replace(/</g, "‹")

    background: Rectangle {
        radius: Theme.radiusSm
        color: tile.chosen ? Theme.accentA(0.10) : (tile.hovered ? Theme.surface2 : Theme.bg)
        border.color: tile.chosen ? Theme.accent : (tile.activeFocus ? Theme.accentFg : (tile.hovered ? Theme.borderStrong : Theme.border))
        border.width: tile.chosen || tile.activeFocus ? 1.5 : 1
    }
    contentItem: Item {
        TypeLogo {
            id: logo
            objectName: "tileLogo"
            anchors.horizontalCenter: parent.horizontalCenter
            anchors.top: parent.top
            anchors.topMargin: 10
            typeId: tile.typeId
            name: tile.text
            size: 30
            chosen: tile.chosen
        }
        Text {
            objectName: "tileName"
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.leftMargin: 6
            anchors.rightMargin: 6
            anchors.top: logo.bottom
            anchors.topMargin: 6
            horizontalAlignment: Text.AlignHCenter
            textFormat: Text.PlainText
            text: tile.text
            color: tile.chosen ? Theme.textStrong : Theme.text
            font.pixelSize: 12
            font.weight: tile.chosen ? Font.DemiBold : Font.Medium
            elide: Text.ElideRight
        }
    }
}
