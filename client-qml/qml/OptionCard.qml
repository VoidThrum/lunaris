import QtQuick
import QtQuick.Controls

// One choice of a few, shown as a card to click instead of a line in a
// drop-down: an initial in a small square, a name and, when there is one,
// a line about it. The chosen one has the accent's border and a dot.
// What it shows can come from another computer: plain text only.
AbstractButton {
    id: option
    // The name; `note` is the line under it ("" for none).
    property string note: ""
    property bool chosen: false
    // No square with an initial (a plain list of names).
    property bool plain: false
    implicitHeight: Math.max(36, column.implicitHeight + 14)
    implicitWidth: 200
    hoverEnabled: true
    activeFocusOnTab: true
    checkable: false
    Accessible.role: Accessible.RadioButton
    Accessible.name: option.text
    Accessible.description: option.note
    Accessible.checked: option.chosen
    Keys.onReturnPressed: option.clicked()
    Keys.onEnterPressed: option.clicked()
    Keys.onSpacePressed: option.clicked()

    background: Rectangle {
        radius: Theme.radiusSm
        color: option.chosen ? Theme.accentA(0.10) : (option.hovered ? Theme.surface2 : Theme.bg)
        border.color: option.chosen ? Theme.accent : (option.activeFocus ? Theme.accentFg : (option.hovered ? Theme.borderStrong : Theme.border))
        border.width: option.chosen || option.activeFocus ? 1.5 : 1
    }
    contentItem: Item {
        Rectangle {
            id: initial
            visible: !option.plain
            width: 24; height: 24; radius: 6
            anchors.left: parent.left
            anchors.leftMargin: 10
            anchors.verticalCenter: parent.verticalCenter
            color: option.chosen ? Theme.accent : Theme.surface3
            Text {
                anchors.centerIn: parent
                textFormat: Text.PlainText
                text: option.text.trim().charAt(0).toUpperCase()
                color: option.chosen ? Theme.textOnAccent : Theme.textMuted
                font.pixelSize: 12
                font.weight: Font.Bold
            }
        }
        Column {
            id: column
            anchors.left: option.plain ? parent.left : initial.right
            anchors.leftMargin: option.plain ? 12 : 9
            anchors.right: dot.left
            anchors.rightMargin: 6
            anchors.verticalCenter: parent.verticalCenter
            spacing: 1
            Text {
                width: parent.width
                textFormat: Text.PlainText
                text: option.text
                color: Theme.textStrong
                font.pixelSize: 13
                font.weight: option.chosen ? Font.DemiBold : Font.Medium
                elide: Text.ElideRight
            }
            Text {
                visible: option.note !== ""
                width: parent.width
                textFormat: Text.PlainText
                text: option.note
                color: Theme.textMuted
                font.pixelSize: 11
                elide: Text.ElideRight
            }
        }
        Rectangle {
            id: dot
            width: 14; height: 14; radius: 7
            anchors.right: parent.right
            anchors.rightMargin: 10
            anchors.verticalCenter: parent.verticalCenter
            color: "transparent"
            border.color: option.chosen ? Theme.accent : Theme.borderStrong
            border.width: 1.5
            Rectangle { anchors.centerIn: parent; width: 6; height: 6; radius: 3; color: Theme.accent; visible: option.chosen }
        }
    }
}
