import QtQuick
import QtQuick.Controls
import QtTest
import "../../qml"

// The "Add computer" flow and the cards of computers of other types,
// rendered off screen with a stub bridge (StubBridge.qml): checks what the
// pages do and saves a picture of each state, in English and Vietnamese,
// to out/. See run.sh. No window is opened and nothing is connected to.
Item {
    id: window
    width: 1180
    height: 760

    // What the pages expect of main.qml's window.
    property bool closeToTray: true
    property bool autostartEnabled: false
    property bool showStats: false
    function startSystemMove() {}

    StubBridge { id: bridge }
    Item { id: fileManager; property var opened: []; property var calls: []; function openFor(server, token, hostId, name, sourceId) { opened = opened.concat([hostId]); calls = calls.concat([[server, token, hostId, name, sourceId || ""]]); } }

    readonly property string outDir: String(Qt.resolvedUrl("out")).replace(/^file:\/\//, "")

    Loader { id: homeLoader; anchors.fill: parent; active: false; sourceComponent: Home {} }

    SignalSpy { id: sessions; signalName: "startSessionRequested" }

    TestCase {
        id: test
        name: "Computers"
        when: windowShown

        property var home: null
        property string lang: "en"

        function start(language) {
            homeLoader.active = false;
            test.lang = language;
            bridge.startOver();
            bridge.stores = {
                app_settings: JSON.stringify({ theme: "dark", language: language }),
                peers: JSON.stringify({ peers: [
                    { address: "192.168.1.20:8091", name: "Office PC", password: "saved", favorite: true, os: "windows", lastUsed: 30 },
                    { address: "source:den-pc-1a2b", name: "Den PC", type: "rustdesk", sourceId: "den-pc-1a2b", favorite: false, lastUsed: 20 },
                    { address: "10.0.0.7:8091", name: "", password: "", favorite: false, os: "", lastUsed: 10 }
                ] })
            };
            homeLoader.active = true;
            test.home = homeLoader.item;
            sessions.target = test.home;
            sessions.clear();
            wait(50);
        }

        function shot(what) {
            // Popups fade in; the picture is of the whole window, overlay included.
            wait(350);
            var image = grabImage(window.Window.window.contentItem);
            var path = window.outDir + "/desktop-" + test.lang + "-" + what + ".png";
            image.save(path);
        }

        function cardOf(address) {
            var grid = findChild(test.home, "peerGrid");
            var items = grid.contentItem.children;
            for (var i = 0; i < items.length; i++) {
                var card = findChild(items[i], "peerCard");
                if (card && card.peer.address === address) return card;
            }
            return null;
        }

        function peer(address) {
            var peers = JSON.parse(bridge.stores.peers).peers;
            for (var i = 0; i < peers.length; i++) if (peers[i].address === address) return peers[i];
            return null;
        }

        function field(dialog, key) { return findChild(dialog.contentItem, "field-" + key); }

        function closeAll(dialog) {
            if (dialog) dialog.close();
            wait(250);
        }

        function test_screens_data() {
            return [{ tag: "en" }, { tag: "vi" }];
        }

        function test_screens(data) {
            start(data.tag);
            var home = test.home;

            // ---- Home: a Lunaris card and a card of another type ----
            var sourceCard = cardOf("source:den-pc-1a2b");
            verify(sourceCard !== null, "the RustDesk-type computer has a card");
            verify(sourceCard.isSource);
            verify(!cardOf("192.168.1.20:8091").isSource);
            // Only Lunaris computers have a port to check.
            wait(100);
            verify(bridge.probed.indexOf("192.168.1.20:8091") !== -1);
            verify(bridge.probed.indexOf("source:den-pc-1a2b") === -1, "a source card is not probed");
            shot("home-list");

            // One click connects, through the app's helper ("bridge:"), with no password.
            mouseClick(sourceCard, sourceCard.width / 2, 40);
            compare(sessions.count, 1);
            compare(sessions.signalArguments[0][0], "bridge:");
            compare(sessions.signalArguments[0][1], "");
            compare(sessions.signalArguments[0][2], "direct");
            compare(sessions.signalArguments[0][3], "Den PC");
            compare(sessions.signalArguments[0][15], "den-pc-1a2b");
            sessions.clear();
            home.setStatus("", false);
            // Connecting moved the card to the front: the list was made anew.
            wait(50);
            sourceCard = cardOf("source:den-pc-1a2b");

            // Its files: the same file manager, through the app's helper,
            // naming the computer as the source; no password is asked for.
            var filesBefore = fileManager.calls.length;
            findChild(sourceCard, "peerFilesButton").clicked();
            compare(fileManager.calls.length, filesBefore + 1);
            compare(fileManager.calls[filesBefore], ["bridge:", "", "direct", "Den PC", "den-pc-1a2b"]);
            compare(sessions.count, 0, "opening the files does not start a session");

            // Its menu: Files, but no Rename and no Forget password; Edit instead.
            var more = findChild(sourceCard, "peerMoreButton");
            more.clicked();
            shot("home-card-menu");
            var menu = findChild(sourceCard, "peerMenu");
            menu.close();
            wait(250);

            // ---- Add computer ----
            var dialog = findChild(home, "addComputerDialog");
            findChild(home, "addComputerButton").clicked();
            verify(dialog.opened);
            compare(dialog.typeOptions.length, 3);
            compare(dialog.typeId, "lunaris");
            findChild(dialog.contentItem, "addressField").text = "192.168.1.31";
            findChild(dialog.contentItem, "lunarisNameField").text = "Meeting room";
            shot("add-lunaris");
            findChild(dialog.contentItem, "submitButton").clicked();
            verify(!dialog.visible || !dialog.opened);
            wait(250);
            var added = peer("192.168.1.31:8091");
            verify(added !== null, "the Lunaris computer is saved");
            compare(added.name, "Meeting room");
            compare(sessions.count, 0, "adding does not connect");

            // The types in the drop-down
            dialog.openAdd();
            var combo = findChild(dialog.contentItem, "typeCombo");
            combo.popup.open();
            shot("add-type-choice");
            combo.popup.close();

            // A connector type: the form is the connector's fields
            dialog.typeId = "rustdesk";
            wait(50);
            compare(field(dialog, "port").text, "21118", "the default is filled in");
            verify(field(dialog, "password").password, "a secret is not readable");
            shot("add-rustdesk-empty");

            // Required fields and the port are checked before anything is saved
            findChild(dialog.contentItem, "nameField").text = "Workshop";
            findChild(dialog.contentItem, "submitButton").clicked();
            verify(dialog.error !== "");
            verify(dialog.opened);
            field(dialog, "host").text = "192.168.1.60";
            field(dialog, "port").text = "70000";
            field(dialog, "password").text = "not-shown";
            findChild(dialog.contentItem, "submitButton").clicked();
            verify(dialog.error !== "");
            shot("add-validation-error");

            field(dialog, "port").text = "21118";
            dialog.error = "";
            shot("add-rustdesk");
            findChild(dialog.contentItem, "submitButton").clicked();
            wait(250);
            verify(!dialog.opened);
            var workshop = peer("source:workshop-0001");
            verify(workshop !== null, "the computer has a card");
            compare(workshop.type, "rustdesk");
            compare(workshop.sourceId, "workshop-0001");
            verify(bridge.stores.peers.indexOf("not-shown") === -1, "the password is not in the list of computers");

            // Another connector, other fields: nothing in the dialog is RustDesk's
            dialog.openAdd();
            dialog.typeId = "acme-view";
            wait(50);
            compare(field(dialog, "gateway").text, "gw.example.net");
            compare(field(dialog, "gateway_port").text, "7400");
            verify(field(dialog, "token").password);
            verify(field(dialog, "host") === null);
            findChild(dialog.contentItem, "nameField").text = "Press room";
            field(dialog, "station").text = "press-04";
            field(dialog, "token").text = "abcdef";
            shot("add-other-type");
            closeAll(dialog);

            // ---- Edit: values that may be shown are there, secrets are not ----
            home.editSource(peer("source:den-pc-1a2b"));
            wait(50);
            verify(dialog.editing);
            compare(findChild(dialog.contentItem, "nameField").text, "Den PC");
            compare(field(dialog, "host").text, "192.168.1.50");
            compare(field(dialog, "password").text, "");
            shot("edit");
            findChild(dialog.contentItem, "nameField").text = "Den";
            findChild(dialog.contentItem, "submitButton").clicked();
            wait(250);
            verify(!dialog.opened, "an empty password keeps the stored one");
            compare(peer("source:den-pc-1a2b").name, "Den");
            home.updatePeer("source:den-pc-1a2b", { name: "Den PC" });

            // ---- Stream settings of a source card: its own displays ----
            home.sessionPeer = "source:den-pc-1a2b";
            home.sessionSource = "den-pc-1a2b";
            home.sourceDisplaysSeen(JSON.stringify([{ id: "0", name: "Display 1", width: 1920, height: 1080 }, { id: "1", name: "Display 2", width: 2560, height: 1440 }]), "0");
            bridge.capabilitiesAsked = [];
            home.openPeerSettings("source:den-pc-1a2b");
            var settings = findChild(home, "peerSettings");
            verify(settings.visible);
            verify(settings.sourceOnly);
            compare(bridge.capabilitiesAsked.length, 0, "nothing is asked of a source card");
            compare(settings.displayOptions.length, 3);
            settings.category = "host";
            settings.displayId = "1";
            settings.save();
            compare(peer("source:den-pc-1a2b").settings.displayId, "1");
            home.requestConnect("source:den-pc-1a2b");
            compare(sessions.signalArguments[0][14], "1", "the chosen display goes with the session");
            sessions.clear();

            // Remove takes the card and what the helper keeps
            home.removeSourcePeer(peer("source:workshop-0001"));
            verify(peer("source:workshop-0001") === null);
            compare(bridge.ownSources.length, 1);

            // ---- Server tab ----
            home.peerTab = "server";
            var dashboard = home.dashboard;
            tryCompare(dashboard, "isAdmin", true);
            compare(dashboard.cards.length, 4);
            shot("server-list");

            // A source card connects as a session to the computer it is reached through
            dashboard.connectSource(dashboard.sourcesList[0]);
            compare(sessions.count, 1);
            compare(sessions.signalArguments[0][0], "http://192.168.1.5:8090");
            compare(sessions.signalArguments[0][2], bridge.hosts[0].id);
            compare(sessions.signalArguments[0][3], "Lab server");
            compare(sessions.signalArguments[0][15], "lab-server-9f31");
            sessions.clear();
            home.setStatus("", false);

            var serverDialog = findChild(dashboard, "serverAddDialog");
            serverDialog.openDialog();
            compare(serverDialog.typeOptions.length, 3);
            shot("server-add-lunaris");
            serverDialog.typeId = "rustdesk";
            wait(50);
            compare(serverDialog.throughHost, bridge.hosts[0].id);
            findChild(serverDialog.contentItem, "nameField").text = "Den PC";
            field(serverDialog, "host").text = "192.168.1.50";
            field(serverDialog, "password").text = "not-shown";
            shot("server-add-connector");

            // The server's own words are shown in the dialog
            bridge.nextWrite = { status: 409, body: JSON.stringify({ error: "That computer asks at its screen before each connection, so computers cannot be added to it from here. Add it in the Lunaris app on that computer." }) };
            findChild(serverDialog.contentItem, "submitButton").clicked();
            verify(serverDialog.opened);
            verify(serverDialog.error !== "");
            shot("server-error");
            bridge.nextWrite = { status: 403, body: "Admin access required" };
            findChild(serverDialog.contentItem, "submitButton").clicked();
            compare(serverDialog.error, I18n.t("Admin access required"));
            shot("server-error-403");

            bridge.calls = [];
            findChild(serverDialog.contentItem, "submitButton").clicked();
            wait(250);
            verify(!serverDialog.opened);
            var post = bridge.calls[0];
            compare(post.method, "POST");
            compare(post.path, "/api/hosts/" + bridge.hosts[0].id + "/sources");
            var sent = JSON.parse(post.body);
            compare(sent.name, "Den PC");
            compare(sent.connector, "rustdesk");
            compare(sent.values.host, "192.168.1.50");
            compare(sent.values.port, "21118");
            compare(dashboard.sourcesList.length, 3);

            // Edit on the server: empty fields, and only what was typed is sent
            serverDialog.openEdit(dashboard.cards[2], ({}));
            wait(50);
            compare(field(serverDialog, "host").text, "");
            field(serverDialog, "host").text = "192.168.1.77";
            shot("server-edit");
            bridge.calls = [];
            findChild(serverDialog.contentItem, "submitButton").clicked();
            wait(250);
            compare(bridge.calls[0].method, "PUT");
            compare(bridge.calls[0].path, "/api/hosts/" + bridge.hosts[0].id + "/sources/lab-server-9f31");
            var changed = JSON.parse(bridge.calls[0].body);
            compare(changed.values.host, "192.168.1.77");
            verify(changed.values.password === undefined);
            verify(changed.values.port === undefined);

            // Remove asks first
            var removeDialog = findChild(dashboard, "removeSourceDialog");
            removeDialog.openFor(dashboard.sourcesList[0]);
            shot("server-remove");
            bridge.calls = [];
            removeDialog.confirm();
            wait(250);
            compare(bridge.calls[0].method, "DELETE");
            compare(bridge.calls[0].path, "/api/hosts/" + bridge.hosts[0].id + "/sources/lab-server-9f31");
            verify(!removeDialog.opened);

            // ---- A server from before this: Lunaris only, no source cards, no error ----
            bridge.oldServer = true;
            bridge.fetchHosts();
            wait(50);
            compare(dashboard.sourcesList.length, 0);
            compare(dashboard.cards.length, 2);
            serverDialog.openDialog();
            compare(serverDialog.typeOptions.length, 1);
            compare(serverDialog.error, "");
            shot("server-old-add");
            closeAll(serverDialog);
            shot("server-old-list");
        }
    }
}
