import QtQuick

// A list of computers whose search and type filter find none: says so, with
// a button that shows every computer again. (The web client: "no-match".)
Column {
    id: noMatch
    signal cleared()
    spacing: 10
    Icon {
        anchors.horizontalCenter: parent.horizontalCenter
        name: "search"
        size: 30
        color: Theme.textSubtle
    }
    Text {
        objectName: "noMatchTitle"
        width: parent.width
        horizontalAlignment: Text.AlignHCenter
        text: I18n.t("No computer matches")
        color: Theme.textMuted
        font.pixelSize: 14
        font.weight: Font.Medium
        wrapMode: Text.WordWrap
    }
    Text {
        width: parent.width
        horizontalAlignment: Text.AlignHCenter
        text: I18n.t("Try other words, or show every type.")
        color: Theme.textSubtle
        font.pixelSize: 12
        wrapMode: Text.WordWrap
    }
    SecondaryButton {
        objectName: "clearFinding"
        anchors.horizontalCenter: parent.horizontalCenter
        text: I18n.t("Clear search and filter")
        onClicked: noMatch.cleared()
    }
}
