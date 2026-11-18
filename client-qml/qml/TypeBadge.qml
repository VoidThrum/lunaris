import QtQuick

// The type of a computer that is not a Lunaris one, as a small pill: the
// connector's name without what it has in brackets ("RustDesk (by IP
// address)" reads "RustDesk"). `onColor` is for a coloured background (a
// card's top), where it is white on a dark veil.
Rectangle {
    id: badge
    property string typeName: ""
    property bool onColor: false
    property real maxWidth: 160
    readonly property string shortName: I18n.t(typeName).replace(/\s*[(（].*$/, "")
    visible: shortName !== ""
    implicitWidth: Math.min(maxWidth, label.implicitWidth + 14)
    implicitHeight: 18
    width: implicitWidth
    height: implicitHeight
    radius: 9
    color: onColor ? Qt.rgba(0, 0, 0, 0.38) : Theme.accentA(0.14)
    border.color: onColor ? Qt.rgba(1, 1, 1, 0.28) : Theme.accentA(0.45)
    border.width: 1
    Accessible.name: I18n.t(typeName)
    Text {
        id: label
        anchors.centerIn: parent
        width: Math.min(implicitWidth, badge.maxWidth - 14)
        // (A connector's name comes from another computer.)
        textFormat: Text.PlainText
        text: badge.shortName
        color: badge.onColor ? "#ffffff" : Theme.accentFg
        font.pixelSize: 10
        font.weight: Font.DemiBold
        elide: Text.ElideRight
    }
}
