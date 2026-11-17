import QtQuick

// Stand-in for the native cursor setter: sets no cursor.
QtObject {
    property Item target: null
    property bool active: false
    property bool hidden: false
    property string source: ""
    property int cursorWidth: 32
    property int cursorHeight: 32
    property int hotspotX: 0
    property int hotspotY: 0
}
