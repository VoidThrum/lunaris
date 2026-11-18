import QtQuick
import QtQuick.Window
import QtTest
import "../../qml"

// main.qml itself, off screen, with the stand-ins of stubs/: when the keys
// held on the remote computer are let go, and when the window system is
// asked for the system's shortcuts (ViewPrefs.systemKeys), for each way a
// window system gives them (the bridge's systemKeysSupport: an X11 grab, a
// Wayland compositor, neither). No key is pressed: the window being active,
// the stream view having the focus and the pointer being over the picture
// are set by hand.
Item {
    id: root
    width: 200
    height: 100

    TestCase {
        name: "Keyboard"
        when: windowShown

        property var app: null
        property var bridge: null

        function init() {
            ViewPrefs.systemKeys = "always";
            var component = Qt.createComponent("../../qml/main.qml");
            compare(component.status, Component.Ready, component.errorString());
            app = component.createObject(null);
            verify(app !== null);
            bridge = findChild(app, "bridge");
            verify(bridge !== null);
            app.windowActive = false;
            app.streamViewFocused = false;
            app.pointerOverStream = false;
        }

        function cleanup() {
            if (app) app.destroy();
            app = null;
            ViewPrefs.systemKeys = "always";
        }

        // The stream view has the keyboard: the window is active, the view focused.
        function giveKeyboard() {
            app.isStreamMode = true;
            app.windowActive = true;
            app.streamViewFocused = true;
            verify(app.streamHasKeyboard);
        }

        function lastGrab() {
            return bridge.keyboardGrabs.length ? bridge.keyboardGrabs[bridge.keyboardGrabs.length - 1] : false;
        }

        function test_default_is_always() {
            compare(ViewPrefs.systemKeys, "always");
        }

        function test_held_keys_are_let_go_when_the_window_is_left() {
            giveKeyboard();
            compare(bridge.keysReleased, 0);
            // Alt+Tab taken by this computer: another window is active now.
            app.windowActive = false;
            compare(bridge.keysReleased, 1);
            app.windowActive = true;
            compare(bridge.keysReleased, 1, "coming back lets nothing go");
            // A dialog of the app takes the focus.
            app.streamViewFocused = false;
            compare(bridge.keysReleased, 2);
            app.streamViewFocused = true;
            // The stream ends.
            app.isStreamMode = false;
            compare(bridge.keysReleased, 3);
        }

        function test_the_rule_data() {
            return [
                // kind, mode, shown, hasKeyboard, paused, pointerLocked, fullScreen, pointerOver -> wanted
                { tag: "grab: off", args: ["grab", "off", true, true, false, true, true, true], wanted: false },
                { tag: "grab: no keyboard", args: ["grab", "always", true, false, false, true, true, true], wanted: false },
                { tag: "grab: paused", args: ["grab", "always", true, true, true, true, true, true], wanted: false },
                { tag: "grab: always, pointer over", args: ["grab", "always", true, true, false, false, false, true], wanted: true },
                { tag: "grab: always, pointer away", args: ["grab", "always", true, true, false, false, false, false], wanted: false },
                { tag: "grab: always, pointer locked", args: ["grab", "always", true, true, false, true, false, false], wanted: true },
                { tag: "grab: always, full screen", args: ["grab", "always", true, true, false, false, true, false], wanted: true },
                { tag: "grab: fullscreen, window", args: ["grab", "fullscreen", true, true, false, true, false, true], wanted: false },
                { tag: "grab: fullscreen, full screen", args: ["grab", "fullscreen", true, true, false, false, true, false], wanted: true },
                { tag: "xwayland as grab", args: ["grab-maybe", "always", true, true, false, false, false, true], wanted: true },
                // A Wayland compositor: neither the window being active nor the pointer counts.
                { tag: "compositor: always", args: ["compositor", "always", true, false, false, false, false, false], wanted: true },
                { tag: "compositor: not shown", args: ["compositor", "always", false, true, false, true, true, true], wanted: false },
                { tag: "compositor: paused", args: ["compositor", "always", true, true, true, true, true, true], wanted: false },
                { tag: "compositor: off", args: ["compositor", "off", true, true, false, true, true, true], wanted: false },
                { tag: "compositor: fullscreen, window", args: ["compositor", "fullscreen", true, true, false, true, false, true], wanted: false },
                { tag: "compositor: fullscreen, full screen", args: ["compositor", "fullscreen", true, false, false, false, true, false], wanted: true },
                // No system shortcut can be taken: as before, Qt's grab with the pointer lock.
                { tag: "window: pointer locked", args: ["window", "off", true, true, false, true, false, false], wanted: true },
                { tag: "window: not locked", args: ["window", "always", true, true, false, false, true, true], wanted: false },
                { tag: "none", args: ["no", "always", true, true, false, true, true, true], wanted: false }
            ];
        }

        function test_the_rule(data) {
            compare(app.wantsSystemKeys.apply(app, data.args), data.wanted);
        }

        // Wayland: the request stays while the stream is shown. GNOME's
        // question takes the keyboard and the pointer from the window;
        // taking the request back then would close the question.
        function test_a_compositor_keeps_the_request_while_the_window_is_not_active() {
            bridge.systemKeys = "compositor";
            app.visible = true;
            app.isStreamMode = true;
            compare(app.systemKeysKind, "compositor");
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            compare(bridge.keyboardGrabs.length, 1);
            app.windowActive = true;
            app.streamViewFocused = true;
            app.pointerOverStream = true;
            app.windowActive = false;
            app.pointerOverStream = false;
            wait(400);
            compare(app.systemKeysGrabbed, true);
            compare(bridge.keyboardGrabs.length, 1, "asked once, never taken back");
            // Ctrl+Alt+Esc takes it back; the window hidden too.
            app.pauseSystemKeys();
            compare(lastGrab(), false);
            app.systemKeysPaused = false;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.visible = false;
            compare(lastGrab(), false);
            // Shown again (a new surface): asked again.
            app.visible = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.isStreamMode = false;
            compare(lastGrab(), false);
            app.visible = false;
        }

        function test_no_way_to_take_them_asks_for_nothing() {
            bridge.systemKeys = "no";
            giveKeyboard();
            app.pointerOverStream = true;
            app.isPointerLocked = true;
            wait(400);
            compare(bridge.keyboardGrabs.length, 0);
            app.isPointerLocked = false;
        }

        function test_inside_the_app_only_grabs_with_the_pointer_lock() {
            bridge.systemKeys = "window";
            giveKeyboard();
            app.pointerOverStream = true;
            wait(400);
            compare(bridge.keyboardGrabs.length, 0);
            app.isPointerLocked = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.isPointerLocked = false;
            compare(lastGrab(), false);
        }

        function test_always_follows_the_pointer_and_the_window() {
            giveKeyboard();
            compare(bridge.keyboardGrabs.length, 0, "pointer not over the picture: nothing asked");
            app.pointerOverStream = true;
            verify(app.systemKeysWanted);
            compare(bridge.keyboardGrabs.length, 0, "asked shortly after, not at once");
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            compare(lastGrab(), true);
            // The pointer leaves the picture: given back at once.
            app.pointerOverStream = false;
            compare(app.systemKeysGrabbed, false);
            compare(lastGrab(), false);
            // Back over it, then another window is activated: given back at
            // once, and the held keys let go.
            app.pointerOverStream = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            var released = bridge.keysReleased;
            app.windowActive = false;
            compare(app.systemKeysGrabbed, false);
            compare(lastGrab(), false);
            compare(bridge.keysReleased, released + 1);
        }

        function test_a_short_visit_asks_for_nothing() {
            giveKeyboard();
            app.pointerOverStream = true;
            app.pointerOverStream = false;
            wait(400);
            compare(bridge.keyboardGrabs.length, 0);
        }

        function test_ctrl_alt_esc_gives_them_back_until_a_click() {
            giveKeyboard();
            app.pointerOverStream = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            // What the Ctrl+Alt+Esc shortcut and holding Esc do.
            app.pauseSystemKeys();
            compare(app.systemKeysGrabbed, false);
            compare(lastGrab(), false);
            wait(400);
            compare(app.systemKeysGrabbed, false, "stays with this computer");
            // A click in the picture (streamMouseArea.onPressed) sends them again.
            app.systemKeysPaused = false;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
        }

        function test_leaving_and_coming_back_ends_the_pause() {
            giveKeyboard();
            app.pointerOverStream = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.pauseSystemKeys();
            app.windowActive = false;
            verify(!app.systemKeysPaused);
            app.windowActive = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
        }

        function test_off_never_asks() {
            ViewPrefs.systemKeys = "off";
            giveKeyboard();
            app.pointerOverStream = true;
            app.isPointerLocked = true;
            wait(400);
            compare(bridge.keyboardGrabs.length, 0);
            app.isPointerLocked = false;
        }

        function test_fullscreen_only_in_full_screen() {
            ViewPrefs.systemKeys = "fullscreen";
            giveKeyboard();
            app.pointerOverStream = true;
            wait(400);
            compare(bridge.keyboardGrabs.length, 0, "a window: nothing asked");
            app.visibility = Window.FullScreen;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.visibility = Window.Windowed;
            compare(app.systemKeysGrabbed, false);
            app.visible = false;
        }

        function test_changing_the_setting_applies_at_once() {
            giveKeyboard();
            app.pointerOverStream = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            ViewPrefs.systemKeys = "off";
            compare(app.systemKeysGrabbed, false);
            compare(lastGrab(), false);
        }

        function test_the_stream_ending_gives_them_back() {
            giveKeyboard();
            app.pointerOverStream = true;
            tryCompare(app, "systemKeysGrabbed", true, 2000);
            app.isStreamMode = false;
            compare(app.systemKeysGrabbed, false);
            compare(lastGrab(), false);
        }

        // "Send keys" in the stream menu: down in order, up in the opposite one.
        function test_a_combination_is_pressed_and_let_go() {
            app.isStreamMode = true;
            app.sendKeyCombo([Qt.Key_Control, Qt.Key_Shift, Qt.Key_Escape]);
            compare(bridge.keyEvents, [
                [Qt.Key_Control, true], [Qt.Key_Shift, true], [Qt.Key_Escape, true],
                [Qt.Key_Escape, false], [Qt.Key_Shift, false], [Qt.Key_Control, false]
            ]);
        }

        function test_the_menu_sends_what_it_names() {
            app.isStreamMode = true;
            var menu = findChild(app, "sendKeysMenu");
            verify(menu !== null);
            menu.send([Qt.Key_Alt, Qt.Key_Tab]);
            compare(bridge.keyEvents, [[Qt.Key_Alt, true], [Qt.Key_Tab, true], [Qt.Key_Tab, false], [Qt.Key_Alt, false]]);
        }

        function test_ctrl_alt_del_is_not_offered_for_a_windows_computer() {
            var item = findChild(app, "sendCtrlAltDel");
            verify(item !== null);
            bridge.hostOsUpdated("linux");
            verify(item.offered);
            bridge.hostOsUpdated("Windows");
            verify(!item.offered);
        }

        function test_the_setting_is_saved_and_read() {
            ViewPrefs.systemKeys = "fullscreen";
            var saved = ViewPrefs.toJson();
            ViewPrefs.systemKeys = "always";
            ViewPrefs.fromJson(saved);
            compare(ViewPrefs.systemKeys, "fullscreen");
            ViewPrefs.fromJson(JSON.stringify({ systemKeys: "everything" }));
            compare(ViewPrefs.systemKeys, "fullscreen", "an unknown value changes nothing");
            ViewPrefs.fromJson(JSON.stringify({ displayMode: "fit" }));
            compare(ViewPrefs.systemKeys, "fullscreen", "settings saved before this one keep the value");
        }
    }
}
