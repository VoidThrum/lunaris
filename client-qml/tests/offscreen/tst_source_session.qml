import QtQuick
import QtTest
import "../../qml"

// main.qml itself, off screen, with the stand-ins of stubs/ for the native
// module: a click on a card of another type starts a "bridge:" session,
// and what the host says about the source (it failed, it ended, its
// displays) ends up as text on the home screen and in the card's settings.
Item {
    id: root
    width: 200
    height: 100

    readonly property string outDir: String(Qt.resolvedUrl("out")).replace(/^file:\/\//, "")

    TestCase {
        name: "SourceSession"
        when: windowShown

        property var app: null
        property var bridge: null

        function open(language) {
            if (app) app.destroy();
            var component = Qt.createComponent("../../qml/main.qml");
            compare(component.status, Component.Ready, component.errorString());
            // The stores are read while main.qml is made: the stub reads them
            // from the creation context.
            app = component.createObject(null);
            verify(app !== null);
            bridge = findChild(app, "bridge");
            verify(bridge !== null);
            bridge.stores = {
                app_settings: JSON.stringify({ theme: "dark", language: language }),
                peers: JSON.stringify({ peers: [
                    { address: "192.168.1.20:8091", name: "Office PC", password: "saved", favorite: true, os: "windows", lastUsed: 30 },
                    { address: "source:den-pc-1a2b", name: "Den PC", type: "rustdesk", sourceId: "den-pc-1a2b", favorite: false, lastUsed: 20 }
                ] })
            };
            var home = findChild(app, "home");
            home.loadAll();
            wait(100);
            return home;
        }

        function shot(language, what) {
            wait(300);
            var image = grabImage(app.contentItem);
            image.save(root.outDir + "/desktop-" + language + "-" + what + ".png");
        }

        function sourceCard(home) {
            var items = findChild(home, "peerGrid").contentItem.children;
            for (var i = 0; i < items.length; i++) {
                var card = findChild(items[i], "peerCard");
                if (card && card.peer.address === "source:den-pc-1a2b") return card;
            }
            return null;
        }

        function test_source_session_data() {
            return [{ tag: "en" }, { tag: "vi" }];
        }

        function test_source_session(data) {
            var home = open(data.tag);
            var card = sourceCard(home);
            verify(card !== null);

            // A click on the card: main.qml asks the bridge for a "bridge:" session.
            findChild(card, "peerCardArea").clicked(null);
            compare(bridge.sessions.length, 1);
            var session = bridge.sessions[0];
            compare(session.server, "bridge:");
            compare(session.token, "");
            compare(session.hostId, "direct");
            compare(session.sourceId, "den-pc-1a2b");
            verify(app.isStreamMode);

            // The source lists its displays during the session: kept for its settings.
            bridge.sourceDisplaysUpdated(JSON.stringify([{ id: "0", name: "Display 1", width: 1920, height: 1080 }, { id: "1", name: "Display 2", width: 2560, height: 1440 }]), "0");

            // The host: the source could not be connected.
            bridge.sourceStatus("failed", "Wrong password for the RustDesk computer");
            verify(!app.isStreamMode, "back on the home screen");
            compare(bridge.stopped, 1);
            verify(home.statusIsError);
            compare(home.statusText, I18n.t("Could not connect to the other computer: %1").arg(I18n.t("Wrong password for the RustDesk computer")));
            shot(data.tag, "stream-failed");

            // A reason with a detail of its own is translated around it.
            app.isStreamMode = true;
            bridge.sourceStatus("ended", "The connection to the computer broke: timed out");
            verify(home.statusText.indexOf("timed out") !== -1);
            compare(home.statusText, I18n.t("The other computer closed the session: %1").arg(I18n.t("The connection to the computer broke: %1").arg("timed out")));
            shot(data.tag, "stream-ended");

            // The helper could not be started: the stream task's own error.
            app.isStreamMode = true;
            bridge.streamFailed("That computer is not there any more");
            compare(home.statusText, I18n.t("That computer is not there any more"));

            // The displays it listed are the choice in its stream settings.
            home.setStatus("", false);
            home.openPeerSettings("source:den-pc-1a2b");
            var settings = findChild(home, "peerSettings");
            compare(settings.displayOptions.length, 3);
            settings.category = "host";
            var combo = findChild(settings, "displayCombo");
            combo.popup.open();
            shot(data.tag, "settings-source-displays");
            combo.popup.close();
            settings.close();
        }
    }
}
