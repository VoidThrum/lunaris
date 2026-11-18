import QtQuick
import "Cards.js" as Cards

// The logo of a type of computer: Lunaris' own, RustDesk's, Sunshine's (by
// connector id, Cards.typeLogo); any other type, and a logo that cannot be
// shown (a package without the SVG image plugin), is the first letter of
// its name. With `tile` it stands on a neutral rounded square, so that it
// reads on the light and on the dark theme alike.
// The name can come from another computer: plain text only.
Item {
    id: logo
    // "lunaris" or a connector's id, and the type's name (for the letter).
    property string typeId: ""
    property string name: ""
    property int size: 28
    property bool tile: true
    // The letter's square has the accent's colour (a chosen option).
    property bool chosen: false
    // No letter where there is no logo: nothing at all (a chip).
    property bool letter: true
    // The picture's file, beside this one ("" = none).
    property string file: Cards.typeLogo(typeId)
    // Whether the logo itself is shown (else the letter, or nothing).
    readonly property bool shown: picture.status === Image.Ready
    implicitWidth: shown || letter ? size : 0
    implicitHeight: size
    width: implicitWidth
    height: implicitHeight

    Rectangle {
        anchors.fill: parent
        visible: logo.shown ? logo.tile : logo.letter
        radius: Math.max(3, Math.round(logo.size * 0.22))
        color: logo.shown ? (Theme.dark ? "#26262c" : "#ffffff") : (logo.chosen ? Theme.accent : Theme.surface3)
        border.color: logo.shown ? Theme.border : "transparent"
        border.width: logo.shown ? 1 : 0
    }
    Image {
        id: picture
        objectName: "typeLogoPicture"
        anchors.centerIn: parent
        readonly property int side: logo.tile ? Math.round(logo.size * 0.72) : logo.size
        width: side
        height: side
        visible: logo.shown
        source: logo.file !== "" ? Qt.resolvedUrl(logo.file) : ""
        // Sharp at twice the size, for a screen that scales.
        sourceSize.width: side * 2
        sourceSize.height: side * 2
        fillMode: Image.PreserveAspectFit
        smooth: true
        mipmap: true
    }
    Text {
        objectName: "typeLogoLetter"
        anchors.centerIn: parent
        visible: !logo.shown && logo.letter
        textFormat: Text.PlainText
        text: logo.name.trim().charAt(0).toUpperCase()
        color: logo.chosen ? Theme.textOnAccent : Theme.textMuted
        font.pixelSize: Math.max(9, Math.round(logo.size * 0.46))
        font.weight: Font.Bold
    }
}
