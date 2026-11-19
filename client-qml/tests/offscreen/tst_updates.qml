import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtTest
import "../../qml"

// The update notice above Home (UpdateBanner.qml) and Settings → Updates
// (UpdatesPanel.qml), off screen with the stub bridge: what shows in each
// state of the updater, what the buttons ask for, and pictures in English
// and Vietnamese to out/. No window is opened and nothing is downloaded.
Item {
    id: window
    width: 720
    height: 520

    StubBridge { id: bridge }

    Rectangle { anchors.fill: parent; color: Theme.bg }

    ColumnLayout {
        anchors.fill: parent
        spacing: 16
        UpdateBanner {
            id: banner
            Layout.fillWidth: true
            status: bridge.updateState
        }
        Rectangle {
            Layout.fillWidth: true
            Layout.leftMargin: 20
            Layout.rightMargin: 20
            implicitHeight: panel.implicitHeight + 32
            color: Theme.surface
            border.color: Theme.border
            radius: 8
            UpdatesPanel {
                id: panel
                x: 16; y: 16
                width: parent.width - 32
                status: bridge.updateState
            }
        }
        Item { Layout.fillHeight: true }
    }

    readonly property string outDir: String(Qt.resolvedUrl("out")).replace(/^file:\/\//, "")

    function release(extra) {
        var s = { state: "available", current: "0.6.3", latest: "0.6.4",
                  notes: "Fixes\n- Faster video on Intel\n- The agent restarts after an update",
                  releaseUrl: "https://github.com/collyn/lunaris/releases/tag/v0.6.4",
                  canInstall: true, manualReason: "", manualMessage: "", progress: 0,
                  error: "", errorKind: "", autoCheck: true, lastChecked: 0, installKind: "deb" };
        for (var k in extra) s[k] = extra[k];
        return s;
    }

    TestCase {
        name: "Updates"
        when: windowShown

        function init() {
            Theme.mode = "dark";
            I18n.lang = "en";
            banner.dismissed = false;
            banner.expanded = false;
            bridge.updateInstalls = 0;
            bridge.updateChecks = 0;
            bridge.updateAutoChecks = [];
            bridge.updateState = release({ state: "idle", latest: "", notes: "", canInstall: false });
        }

        function child(root, name) { var c = findChild(root, name); verify(c !== null, name); return c; }

        function shot(name) {
            wait(150);
            grabImage(window).save(window.outDir + "/updates-" + name + ".png");
        }

        function test_nothing_shows_without_a_release() {
            verify(!banner.visible);
            compare(child(panel, "updatesVersion").text, "Lunaris 0.6.3");
            verify(!child(panel, "updatesState").visible);
            verify(!child(panel, "updatesInstall").visible);
            // Up to date, and a failed check: not in the banner.
            bridge.updateState = release({ state: "upToDate", latest: "" });
            verify(!banner.visible);
            compare(child(panel, "updatesState").text, "Lunaris is up to date.");
            bridge.updateState = release({ state: "error", latest: "", errorKind: "check", error: "Could not check for updates: timed out" });
            verify(!banner.visible);
            compare(child(panel, "updatesState").text, "Could not check for updates: timed out");
            shot("check-failed-dark-en");
        }

        function test_available_and_update() {
            bridge.updateState = release({});
            verify(banner.visible);
            compare(child(banner, "updateBannerText").text, "Lunaris 0.6.4 is available");
            var install = child(banner, "updateBannerInstall");
            verify(install.visible);
            verify(!child(banner, "updateBannerDownload").visible);
            shot("available-dark-en");
            mouseClick(install);
            compare(bridge.updateInstalls, 1);
            // The panel's button does the same.
            mouseClick(child(panel, "updatesInstall"));
            compare(bridge.updateInstalls, 2);
            compare(child(panel, "updatesInstall").text, "Update to 0.6.4");
            verify(child(panel, "updatesNotes").visible);
            // Release notes in the banner on demand.
            mouseClick(child(banner, "updateBannerNotes"));
            verify(child(banner, "updateBannerNotesText").visible);
            shot("available-notes-dark-en");
        }

        function test_progress_and_restart() {
            bridge.updateState = release({ state: "downloading", progress: 0.42 });
            verify(banner.visible);
            compare(child(banner, "updateBannerText").text, "Downloading Lunaris 0.6.4… 42%");
            verify(child(banner, "updateBannerProgress").visible);
            compare(child(banner, "updateBannerProgress").value, 0.42);
            verify(!child(banner, "updateBannerInstall").visible, "no second install while busy");
            verify(child(panel, "updatesProgress").visible);
            verify(!child(panel, "updatesCheckNow").enabled);
            shot("downloading-dark-en");
            bridge.updateState = release({ state: "verifying" });
            compare(child(banner, "updateBannerText").text, "Checking the signature…");
            bridge.updateState = release({ state: "installing" });
            compare(child(banner, "updateBannerText").text, "Installing Lunaris 0.6.4…");
            bridge.updateState = release({ state: "restart" });
            compare(child(banner, "updateBannerText").text, "Restarting Lunaris…");
        }

        function test_errors() {
            bridge.updateState = release({ state: "error", errorKind: "cancelled", error: "The update was cancelled" });
            verify(banner.visible);
            compare(child(banner, "updateBannerText").text, "The update was cancelled");
            compare(child(banner, "updateBannerInstall").text, "Try again");
            bridge.updateState = release({ state: "error", errorKind: "verify", error: "The downloaded file does not match its checksum: lunaris-client_0.6.4_amd64.deb" });
            compare(child(panel, "updatesState").text, "The downloaded file does not match its checksum: lunaris-client_0.6.4_amd64.deb");
            shot("verify-failed-dark-en");
            I18n.lang = "vi";
            compare(child(banner, "updateBannerText").text, "Tệp đã tải không khớp mã kiểm tra: lunaris-client_0.6.4_amd64.deb");
            bridge.updateState = release({ state: "error", errorKind: "cancelled", error: "The update was cancelled" });
            compare(child(banner, "updateBannerText").text, "Đã huỷ cập nhật");
        }

        function test_notice_only_builds() {
            bridge.updateState = release({ canInstall: false, manualReason: "notSigned",
                manualMessage: "Updates are not signed in this build: download the new version from the release page." });
            verify(banner.visible);
            verify(!child(banner, "updateBannerInstall").visible);
            verify(child(banner, "updateBannerDownload").visible);
            verify(child(banner, "updateBannerManual").visible);
            verify(!child(panel, "updatesInstall").visible);
            compare(child(panel, "updatesReleasePage").text, "Download");
            shot("not-signed-dark-en");
            Theme.mode = "light";
            I18n.lang = "vi";
            compare(child(banner, "updateBannerText").text, "Đã có Lunaris 0.6.4");
            compare(child(banner, "updateBannerManual").text,
                    "Bản dựng này không kiểm tra được chữ ký bản cập nhật: hãy tải bản mới từ trang phát hành.");
            shot("not-signed-light-vi");
        }

        function test_dismiss_check_and_toggle() {
            bridge.updateState = release({});
            verify(banner.visible);
            banner.dismissed = true;
            verify(!banner.visible);
            mouseClick(child(panel, "updatesCheckNow"));
            compare(bridge.updateChecks, 1);
            var auto = child(panel, "updatesAutoCheck");
            verify(auto.checked);
            mouseClick(auto);
            compare(bridge.updateAutoChecks, [false]);
            I18n.lang = "vi";
            compare(auto.text, "Tự động kiểm tra bản cập nhật");
            shot("settings-light-vi");
        }
    }
}
