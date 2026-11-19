import QtQuick
import QtQuick.Controls

// Above a list of computers: a search field, a chip for each type of
// computer in the list (and "All"), how many are shown, and grid or list.
// What it shows is the owner's: it says what was chosen with its signals.
// (The web client has the same row: App.tsx, "computer-finder".)
// Type names can come from another computer: plain text only.
Item {
    id: finder
    property string searchText: ""
    // [{ id, name }] (Cards.typesOf); the chosen one's id, "" = all.
    property var types: []
    property string typeFilter: ""
    property int total: 0
    property int shown: 0
    // "grid" | "list"
    property string view: "grid"
    signal searchEdited(string text)
    signal typeChosen(string id)
    signal viewChosen(string view)

    implicitHeight: Math.max(tools.height, rightSide.height)

    Flow {
        id: tools
        anchors.left: parent.left
        anchors.right: rightSide.left
        anchors.rightMargin: 12
        spacing: 6

        Rectangle {
            id: searchBox
            width: Math.min(240, tools.width)
            height: 30
            radius: Theme.radiusSm
            color: Theme.surface
            border.color: searchInput.activeFocus ? Theme.accent : Theme.border
            Icon { id: searchIcon; name: "search"; size: 14; color: Theme.textSubtle; anchors.left: parent.left; anchors.leftMargin: 9; anchors.verticalCenter: parent.verticalCenter }
            TextInput {
                id: searchInput
                objectName: "computerSearch"
                anchors.left: searchIcon.right
                anchors.leftMargin: 7
                anchors.right: clearSearch.visible ? clearSearch.left : parent.right
                anchors.rightMargin: clearSearch.visible ? 2 : 8
                anchors.verticalCenter: parent.verticalCenter
                color: Theme.text
                selectionColor: Theme.accentA(0.4)
                font.pixelSize: 12
                clip: true
                text: finder.searchText
                onTextEdited: finder.searchEdited(text)
                Keys.onEscapePressed: (event) => {
                    if (text === "") { event.accepted = false; return; }
                    finder.searchEdited("");
                }
                Accessible.name: I18n.t("Search computers")
                Text {
                    visible: searchInput.text.length === 0
                    text: I18n.t("Search by name or address")
                    color: Theme.textSubtle
                    font.pixelSize: 12
                    elide: Text.ElideRight
                    width: searchInput.width
                    anchors.verticalCenter: parent.verticalCenter
                }
            }
            IconButton {
                id: clearSearch
                objectName: "clearSearch"
                visible: finder.searchText !== ""
                anchors.right: parent.right
                anchors.rightMargin: 3
                anchors.verticalCenter: parent.verticalCenter
                implicitWidth: 24
                implicitHeight: 24
                iconName: "close"
                iconSize: 13
                tip: I18n.t("Clear search")
                onClicked: { finder.searchEdited(""); searchInput.forceActiveFocus(); }
            }
        }

        // The types: only when there are two or more to tell apart.
        Repeater {
            model: finder.types.length > 1 ? [{ id: "", name: I18n.t("All") }].concat(finder.types) : []
            delegate: AbstractButton {
                id: chip
                required property var modelData
                readonly property bool chosen: modelData.id === finder.typeFilter
                objectName: "typeChip"
                property string typeId: modelData.id
                height: 30
                implicitWidth: (logo.visible ? logo.width + 6 : 0) + chipText.implicitWidth + 22
                width: Math.min(implicitWidth, tools.width)
                hoverEnabled: true
                activeFocusOnTab: true
                Accessible.role: Accessible.RadioButton
                Accessible.name: modelData.name
                Accessible.checked: chosen
                Keys.onReturnPressed: clicked()
                Keys.onSpacePressed: clicked()
                onClicked: finder.typeChosen(modelData.id)
                background: Rectangle {
                    radius: height / 2
                    color: chip.chosen ? Theme.accentA(0.14) : (chip.hovered ? Theme.surface2 : Theme.surface)
                    border.color: chip.chosen ? Theme.accent : (chip.activeFocus ? Theme.accentFg : Theme.border)
                    border.width: chip.chosen || chip.activeFocus ? 1.5 : 1
                }
                contentItem: Item {
                    TypeLogo {
                        id: logo
                        visible: chip.modelData.id !== ""
                        x: 9
                        anchors.verticalCenter: parent.verticalCenter
                        typeId: chip.modelData.id
                        name: chip.modelData.name
                        size: 16
                        tile: false
                    }
                    Text {
                        id: chipText
                        x: logo.visible ? logo.x + logo.width + 6 : 11
                        width: parent.width - x - 11
                        anchors.verticalCenter: parent.verticalCenter
                        textFormat: Text.PlainText
                        text: chip.modelData.name
                        color: chip.chosen ? Theme.textStrong : Theme.text
                        font.pixelSize: 12
                        font.weight: chip.chosen ? Font.DemiBold : Font.Normal
                        elide: Text.ElideRight
                    }
                }
            }
        }
    }

    Row {
        id: rightSide
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: 10
        Text {
            objectName: "computerCount"
            anchors.verticalCenter: parent.verticalCenter
            text: finder.shown === finder.total
                ? (finder.total === 1 ? I18n.t("1 computer") : I18n.t("%1 computers").arg(finder.total))
                : I18n.t("%1 of %2 computers").arg(finder.shown).arg(finder.total)
            color: Theme.textMuted
            font.pixelSize: 12
        }
        // Grid or list
        Rectangle {
            width: viewRow.width + 4
            height: 30
            radius: Theme.radiusSm
            color: Theme.surface
            border.color: Theme.border
            Row {
                id: viewRow
                anchors.centerIn: parent
                spacing: 2
                Repeater {
                    model: [{ view: "grid", tip: "Grid" }, { view: "list", tip: "List" }]
                    delegate: AbstractButton {
                        id: viewButton
                        required property var modelData
                        readonly property bool chosen: finder.view === modelData.view
                        objectName: "view-" + modelData.view
                        implicitWidth: 26
                        implicitHeight: 24
                        hoverEnabled: true
                        activeFocusOnTab: true
                        Accessible.role: Accessible.RadioButton
                        Accessible.name: I18n.t(modelData.tip)
                        Accessible.checked: chosen
                        ToolTip.visible: hovered
                        ToolTip.delay: 400
                        ToolTip.text: I18n.t(modelData.tip)
                        Keys.onReturnPressed: clicked()
                        Keys.onSpacePressed: clicked()
                        onClicked: finder.viewChosen(modelData.view)
                        background: Rectangle {
                            radius: Theme.radiusSm - 2
                            color: viewButton.chosen ? Theme.accentA(0.16) : (viewButton.hovered ? Theme.surface2 : "transparent")
                            border.color: viewButton.activeFocus ? Theme.accentFg : "transparent"
                        }
                        contentItem: Item {
                            Icon {
                                anchors.centerIn: parent
                                name: viewButton.modelData.view
                                size: 15
                                color: viewButton.chosen ? Theme.accentFg : (viewButton.hovered ? Theme.textStrong : Theme.textMuted)
                            }
                        }
                    }
                }
            }
        }
    }
}
