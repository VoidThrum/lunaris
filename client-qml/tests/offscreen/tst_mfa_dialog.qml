import QtQuick
import QtQuick.Controls
import QtTest
import "../../qml"

// The two-factor step of signing in (MfaDialog.qml), off screen with the
// stub bridge: what is sent for a code and for a recovery code, how the
// server's answers show, and pictures in English and Vietnamese, dark and
// light, to out/. No window is opened and nothing is connected to.
Item {
    id: window
    width: 560
    height: 480

    StubBridge { id: bridge }

    Rectangle { anchors.fill: parent; color: Theme.bg }

    MfaDialog { id: dlg }

    readonly property string outDir: String(Qt.resolvedUrl("out")).replace(/^file:\/\//, "")

    TestCase {
        name: "MfaDialog"
        when: windowShown

        function init() {
            Theme.mode = "dark";
            I18n.lang = "en";
            bridge.mfaCalls = [];
            bridge.mfaCancels = 0;
        }

        function cleanup() {
            dlg.busy = false;
            if (dlg.opened) dlg.close();
            tryCompare(dlg, "visible", false);
        }

        function field() { return findChild(dlg.contentItem, "mfaCodeField"); }
        function errorText() { return findChild(dlg.contentItem, "mfaError"); }

        function shot(name) {
            wait(150);
            var image = grabImage(window);
            image.save(window.outDir + "/mfa-" + name + ".png");
        }

        function test_code_is_sent_as_digits() {
            dlg.begin("alice");
            tryCompare(dlg, "opened", true);
            shot("code-dark-en");
            // Too short: nothing is sent.
            field().text = "123";
            dlg.submit();
            compare(bridge.mfaCalls.length, 0);
            verify(errorText().visible);
            compare(errorText().text, "Enter the 6-digit code from your app");
            // A pasted code with a space.
            field().text = "123 456";
            dlg.submit();
            compare(bridge.mfaCalls.length, 1);
            compare(bridge.mfaCalls[0].code, "123456");
            compare(bridge.mfaCalls[0].recovery, false);
            verify(dlg.busy);
            // A wrong code: the dialog stays, with the server's text.
            bridge.loginResult(false, "Wrong code", "", "", "");
            verify(!dlg.busy);
            verify(dlg.opened);
            compare(errorText().text, "Wrong code");
            compare(field().text, "");
            shot("wrong-code-dark-en");
            // The right one: the dialog closes.
            field().text = "654321";
            dlg.submit();
            bridge.loginResult(true, "", "jwt", "alice", "http://srv");
            tryCompare(dlg, "visible", false);
        }

        function test_recovery_code() {
            dlg.begin("alice");
            tryCompare(dlg, "opened", true);
            mouseClick(findChild(dlg.contentItem, "mfaSwitch"));
            verify(dlg.recovery);
            field().text = "ab";
            dlg.submit();
            compare(bridge.mfaCalls.length, 0);
            field().text = " abcd-efgh-jkmn ";
            dlg.submit();
            compare(bridge.mfaCalls.length, 1);
            compare(bridge.mfaCalls[0].code, "abcd-efgh-jkmn");
            compare(bridge.mfaCalls[0].recovery, true);
            dlg.busy = false;
            shot("recovery-dark-en");
        }

        function test_expired_goes_back() {
            var expired = 0;
            var onExpired = function () { expired += 1; };
            dlg.expired.connect(onExpired);
            dlg.begin("alice");
            tryCompare(dlg, "opened", true);
            field().text = "123456";
            dlg.submit();
            bridge.loginResult(false, "This sign-in has expired. Sign in again.", "", "", "");
            tryCompare(dlg, "visible", false);
            compare(expired, 1);
            dlg.expired.disconnect(onExpired);
        }

        function test_back_drops_the_sign_in() {
            dlg.begin("alice");
            tryCompare(dlg, "opened", true);
            dlg.cancel();
            tryCompare(dlg, "visible", false);
            verify(bridge.mfaCancels >= 1);
        }

        function test_vietnamese_and_light() {
            I18n.lang = "vi";
            dlg.begin("alice");
            tryCompare(dlg, "opened", true);
            shot("code-dark-vi");
            compare(findChild(dlg.contentItem, "mfaVerify").text, "Xác minh");
            field().text = "1";
            dlg.submit();
            compare(errorText().text, "Nhập mã 6 chữ số từ ứng dụng");
            Theme.mode = "light";
            shot("code-light-vi");
            Theme.mode = "dark";
        }
    }
}
