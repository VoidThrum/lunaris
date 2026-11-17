import QtQuick
import QtTest

// main.qml itself, off screen, with the stand-ins of stubs/: the viewer's
// own pointer is hidden while a button is held and the host draws its
// pointer into the picture. Either the host says so (a Windows host while a
// window is moved), or the client found the pointer in the decoded picture
// (pointerInPictureUpdated with found). The host's "maybe" alone hides
// nothing.
Item {
    id: root
    width: 200
    height: 100

    TestCase {
        name: "PointerInPicture"
        when: windowShown

        property var app: null
        property var bridge: null

        function init() {
            var component = Qt.createComponent("../../qml/main.qml");
            compare(component.status, Component.Ready, component.errorString());
            app = component.createObject(null);
            verify(app !== null);
            bridge = findChild(app, "bridge");
            verify(bridge !== null);
        }

        function cleanup() {
            if (app) app.destroy();
            app = null;
        }

        function test_maybe_alone_hides_nothing() {
            bridge.hostOsUpdated("Windows");
            app.hostCursorMouseDown = true;
            bridge.hostCursorUpdated(100, 100, true, "arrow", false);
            bridge.pointerInPictureUpdated(true, false);
            verify(app.pointerMaybeInPicture);
            verify(!app.pointerFoundInPicture);
            verify(!app.shouldHidePredictedCursor(), "only \"maybe\": the viewer's pointer stays");
        }

        function test_found_hides_while_the_button_is_held_data() {
            // Whatever the host's system is said to be: the picture was looked at.
            return [{ tag: "windows" }, { tag: "linux" }, { tag: "unknown" }];
        }

        function test_found_hides_while_the_button_is_held(data) {
            bridge.hostOsUpdated(data.tag);
            bridge.hostCursorUpdated(100, 100, true, "arrow", false);
            verify(!app.shouldHidePredictedCursor());

            app.hostCursorMouseDown = true;
            verify(!app.shouldHidePredictedCursor());
            bridge.pointerInPictureUpdated(true, false);
            verify(!app.shouldHidePredictedCursor());
            bridge.pointerInPictureUpdated(true, true);
            verify(app.shouldHidePredictedCursor(), "found in the picture: hidden");

            // The host's "maybe" went false, or the button was let go: shown again.
            bridge.pointerInPictureUpdated(false, false);
            verify(!app.shouldHidePredictedCursor());
            bridge.pointerInPictureUpdated(true, true);
            verify(app.shouldHidePredictedCursor());
            app.hostCursorMouseDown = false;
            verify(!app.shouldHidePredictedCursor(), "no button held: never hidden");
        }

        function test_the_hosts_own_flag_still_hides() {
            bridge.hostOsUpdated("Windows");
            app.hostCursorMouseDown = true;
            bridge.hostCursorUpdated(100, 100, true, "arrow", true);
            verify(app.shouldHidePredictedCursor(), "in_window_move_size of a Windows host");
            bridge.hostCursorUpdated(100, 100, true, "arrow", false);
            verify(!app.shouldHidePredictedCursor());
            // As before, that flag counts for a Windows host only.
            bridge.hostOsUpdated("linux");
            bridge.hostCursorUpdated(100, 100, true, "arrow", true);
            verify(!app.shouldHidePredictedCursor());
        }
    }
}
