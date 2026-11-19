import QtQuick
import QtQuick.Controls

// One type of computer in the types of "Add computer": its logo and its
// short name ("RustDesk") side by side; the whole name is its tool tip and
// what a screen reader says. The chosen one has the accent's border.
// `centered`: logo and name in the middle (a chip in a row of chips);
// otherwise at the left (a row of the list of types beside the form).
// The names can come from another computer: plain text only.
AbstractButton {
    id: tile
    // `text` is the short name.
    property string typeId: ""
    property string fullName: ""
    property bool chosen: false
    property bool centered: false
    property int logoSize: centered ? 20 : 24
    implicitWidth: 120
    implicitHeight: centered ? 38 : 40
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
        Row {
            id: row
            spacing: 8
            // Centered when it fits, else from the left with the name cut.
            x: tile.centered ? Math.max(8, (parent.width - width) / 2) : 8
            anchors.verticalCenter: parent.verticalCenter
            TypeLogo {
                id: logo
                objectName: "tileLogo"
                anchors.verticalCenter: parent.verticalCenter
                typeId: tile.typeId
                name: tile.text
                size: tile.logoSize
                chosen: tile.chosen
            }
            Text {
                objectName: "tileName"
                anchors.verticalCenter: parent.verticalCenter
                width: Math.min(implicitWidth, tile.availableWidth - 16 - logo.width - row.spacing)
                textFormat: Text.PlainText
                text: tile.text
                color: tile.chosen ? Theme.textStrong : Theme.text
                font.pixelSize: 13
                font.weight: tile.chosen ? Font.DemiBold : Font.Medium
                elide: Text.ElideRight
            }
        }
    }
}
