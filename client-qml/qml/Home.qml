import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import "Cards.js" as Cards

// The app's main screen: "This computer" (let others control it) on the
// left, "Control a remote computer" with recent, favorite and server
// computers on the right, and the settings.
Rectangle {
    id: home
    objectName: "home"
    color: Theme.bg

    signal startSessionRequested(string server, string token, string hostId, string hostName, int appId, string res, int fps, string codec, int bitrate, int queueLimit, bool disableCuda, string renderBackend, string inputProtocol, string encoder, string displayId, string sourceId)

    // The server sign-in and its computers (Server tab); main.qml reaches it
    // as `dashboardView`.
    property alias dashboard: serverDashboard

    // "home" | "settings"
    property string page: "home"
    // "recent" | "favorites" | "server"
    property string peerTab: "recent"
    property string searchText: ""

    // This computer (see host.rs): enabled, running, port, password,
    // addresses, hostname, error.
    property var hostStatus: ({ enabled: false, running: false, port: 8091, password: "", addresses: [], hostname: "", input_ready: true, error: "" })
    property bool passwordVisible: false
    property string hostError: ""
    property bool showAllAddresses: false

    // The app's own server (see local_server.rs): enabled, running, ours,
    // port, local_url, lan_url, admin_password, data_dir, error.
    property var serverStatus: ({ enabled: false, running: false, ours: false, port: 8090, local_url: "", lan_url: "", admin_password: "", data_dir: "", error: "" })
    property string serverError: ""
    property bool serverBusy: false

    // Saved computers: [{ address, name, password, favorite, lastUsed, os }].
    // A computer of another type (added with a connector) is
    // { address: "source:<id>", name, type: <connector id>, sourceId, favorite,
    // lastUsed }: its fields are kept by the app's helper, not here.
    property var peers: []
    // The types a computer can be added with besides Lunaris (connectors):
    // [{ id, name, fields }].
    property var connectors: []
    // The app's own computers of other types, as the helper's config has
    // them: [{ id, name, connector, connector_name, values }] (no secrets).
    property var ownSources: []
    // The saved computer the running session is to ("" for one of the
    // Server tab), and the source it shows, for the displays it lists.
    property string sessionPeer: ""
    property string sessionSource: ""

    // Defaults for new connections (Settings > Display), saved in app_settings.
    property var prefs: ({
        theme: "system",
        res: "1920x1080",
        fps: 60,
        codec: "auto",
        bitrate: 15000,
        renderBackend: "auto_gpu"
    })
    // Until a frame rate is picked in the settings, new connections use the
    // refresh rate of the screen this window is on (read again when the
    // window moves to another screen and before each connection).
    property bool fpsFollowsScreen: true
    readonly property string screenName: Screen.name
    onScreenNameChanged: applyScreenFps()
    function screenFps() { return StreamOptions.displayFps(bridge.screenRefreshRate()) }
    function applyScreenFps() {
        if (!home.fpsFollowsScreen) return;
        var rate = screenFps();
        if (home.prefs.fps === rate) return;
        var next = Object.assign({}, home.prefs);
        next.fps = rate;
        home.prefs = next;
        console.log("Default frame rate follows the screen: " + rate + " fps");
    }
    // What goes to app_settings: without the frame rate while it follows the screen.
    function storedPrefs() {
        var out = Object.assign({}, home.prefs);
        if (home.fpsFollowsScreen) delete out.fps;
        return out;
    }

    property string statusText: ""
    property bool statusIsError: false

    // ---------------------------------------------------------------
    // State and persistence
    // ---------------------------------------------------------------
    function loadAll() {
        var stored = bridge.loadStore("app_settings");
        if (stored !== "") {
            try {
                var p = JSON.parse(stored);
                var merged = Object.assign({}, home.prefs);
                for (var k in p) merged[k] = p[k];
                home.prefs = merged;
                home.fpsFollowsScreen = p.fps === undefined;
            } catch (e) { console.log("app_settings: " + e); }
        }
        applyScreenFps();
        Theme.mode = home.prefs.theme || "system";
        I18n.lang = home.prefs.language === "vi" ? "vi" : "en";
        window.closeToTray = home.prefs.closeToTray !== false;
        // The way used last for each card of the Server tab (kept here,
        // with the app's settings).
        serverDashboard.cardMethods = home.prefs.serverCardMethods || ({});
        var storedPeers = bridge.loadStore("peers");
        if (storedPeers !== "") {
            try { home.peers = JSON.parse(storedPeers).peers || []; } catch (e) { console.log("peers: " + e); }
        }
        // Earlier versions remembered one direct connection.
        var lastAddress = bridge.savedDirectAddress();
        if (lastAddress !== "" && findPeer(lastAddress) === null) {
            rememberPeer(lastAddress, bridge.savedDirectPassword(), true);
        }
        refreshOwnComputers();
        adoptOwnComputers();
        refreshHost();
    }

    // ---- Computers of other types (connectors) ----
    function isSource(peer) {
        return !!peer && String(peer.address || "").indexOf("source:") === 0;
    }

    // ---- One card for each computer (Cards.js) ----
    // A saved Lunaris computer and the computers of other types that are
    // the same machine are one card, with a way to connect for each. Which
    // are one machine is said by a group (an opaque word): a computer of
    // another type has it from the app's helper (`ownSources`, where one
    // added at the address of another gets the other's), a Lunaris computer
    // has it in the list (`group`, given when a way is first added to it).
    // A card: { key, title, host, group, methods, peers, favorite, lastUsed }.
    function peerCards() {
        var hosts = [];
        var sources = [];
        for (var i = 0; i < home.peers.length; i++) {
            var p = home.peers[i];
            if (isSource(p)) {
                var s = ownSource(p.sourceId || "");
                sources.push({ host_id: "", host_name: "", id: String(p.sourceId || ""), name: p.name || (s ? s.name : ""),
                               connector: String(p.type || ""), connector_name: typeName(p), online: true,
                               group: s && s.group ? String(s.group) : "", peer: p });
            } else {
                hosts.push({ id: p.address, name: p.name ? p.name : displayAddress(p.address || ""), status: "Online", peer: p });
            }
        }
        var cards = Cards.buildCards(hosts, sources,
            function (source) { return Cards.typeShortName(source.connector, I18n.t(source.connector_name)); }, "Lunaris",
            function (host) { return String(host.peer.group || ""); });
        for (var c = 0; c < cards.length; c++) {
            var members = cards[c].methods.map(function (m) { return methodPeer(m); });
            cards[c].peers = members;
            cards[c].favorite = !!members[0].favorite;
            cards[c].lastUsed = members.reduce(function (latest, peer) { return Math.max(latest, peer.lastUsed || 0); }, 0);
        }
        return cards;
    }
    function methodPeer(method) { return method.host ? method.host.peer : method.source.peer; }
    function peerCardByKey(key) {
        var cards = peerCards();
        for (var i = 0; i < cards.length; i++) if (cards[i].key === key) return cards[i];
        return null;
    }
    // The way a card connects with now, and choosing another (remembered
    // with the app's settings).
    function methodOf(card) { return Cards.currentMethod(card.methods, (home.prefs.cardMethods || {})[card.key]); }
    function chooseMethod(card, methodKey) {
        if ((home.prefs.cardMethods || {})[card.key] === methodKey) return;
        setPref("cardMethods", Cards.remembered(home.prefs.cardMethods || {}, card.key, methodKey));
    }
    function connectMethod(card, method) {
        if (!method) return;
        chooseMethod(card, method.key);
        requestConnect(methodPeer(method).address);
    }
    // The cards a computer of another type can be said to be the same
    // computer as, and the one it is on now ("" for a card of its own).
    function sameOptionsFor(peer) {
        var cards = peerCards();
        var options = [];
        var now = "";
        for (var i = 0; i < cards.length; i++) {
            var mine = cards[i].peers.some(function (p) { return p.address === peer.address; });
            if (mine && cards[i].methods.length === 1) continue;
            if (mine) now = cards[i].key;
            options.push({ value: cards[i].key, label: cards[i].title, group: cards[i].group });
        }
        return { options: options, now: now };
    }
    // The group that was said with a form (AddComputerDialog), as the
    // helper takes it. "@<card key>" is a card that has no group yet: it is
    // given a new one first. Returns { group, error }.
    function groupFor(said) {
        if (said === undefined || said === null) return { group: "*", error: "" };
        if (String(said).charAt(0) !== "@") return { group: String(said), error: "" };
        var card = peerCardByKey(String(said).slice(1));
        if (!card || card.methods.length !== 1) return { group: "*", error: I18n.t("That computer is not there any more") };
        var group = Cards.newGroup();
        var first = card.peers[0];
        if (!isSource(first)) {
            updatePeer(first.address, { group: group });
            return { group: group, error: "" };
        }
        var answer;
        try { answer = JSON.parse(bridge.bridgeUpdateSourceGrouped(first.sourceId, first.name || card.title, "{}", group)); } catch (e) { answer = null; }
        if (!answer || answer.error) return { group: "*", error: I18n.tError(String((answer && answer.error) || "The computer could not be saved")) };
        return { group: group, error: "" };
    }
    function addMethodTo(card) {
        refreshOwnComputers();
        addDialog.openAddMethod({ key: card.key, title: card.title, group: card.group });
    }

    function refreshOwnComputers() {
        try { home.connectors = JSON.parse(bridge.bridgeConnectors()) || []; } catch (e) { console.log("connectors: " + e); }
        try { home.ownSources = JSON.parse(bridge.bridgeSources()) || []; } catch (e) { console.log("own computers: " + e); }
    }

    // A computer the helper has but the list has not (the list was lost or
    // comes from another copy) gets its card back.
    function adoptOwnComputers() {
        var list = home.peers.slice();
        var added = false;
        for (var i = 0; i < home.ownSources.length; i++) {
            var s = home.ownSources[i];
            if (findPeer("source:" + s.id) !== null) continue;
            list.push({ address: "source:" + s.id, name: s.name, type: s.connector, sourceId: s.id, favorite: false, lastUsed: 0 });
            added = true;
        }
        if (added) { home.peers = list; savePeers(); }
    }

    // Whether a computer of another type has files to offer: yes unless
    // its connector says it has none (a type that is not installed any
    // more says nothing).
    function sourceHasFiles(peer) {
        var c = connectorById(peer.type || "");
        return !(c && c.capabilities && c.capabilities.files === false);
    }

    function ownSource(id) {
        for (var i = 0; i < home.ownSources.length; i++)
            if (home.ownSources[i].id === id) return home.ownSources[i];
        return null;
    }

    function connectorById(id) {
        for (var i = 0; i < home.connectors.length; i++)
            if (home.connectors[i].id === id) return home.connectors[i];
        return null;
    }

    // The name of a card's type, for its badge.
    function typeName(peer) {
        var c = connectorById(peer.type || "");
        if (c) return c.name;
        var s = ownSource(peer.sourceId || "");
        return s ? s.connector_name : String(peer.type || "");
    }

    // What stands where a Lunaris card has its address: the first field of
    // the computer that may be shown (for most types, where it is).
    function sourceSummary(peer) {
        var s = ownSource(peer.sourceId || "");
        var c = connectorById(peer.type || "");
        if (s && c) {
            for (var i = 0; i < c.fields.length; i++) {
                var value = s.values ? s.values[c.fields[i].key] : "";
                if (c.fields[i].kind !== "secret" && value) return String(value);
            }
        }
        return I18n.t(typeName(peer));
    }

    // "Add computer", type Lunaris: saved without connecting. Returns an
    // error message or "".
    function addLunarisPeer(address, password, name) {
        var addr = normalizeAddress(address);
        if (addr === "") return I18n.t("Enter the address of the computer");
        var changes = { lastUsed: Date.now() };
        if (name !== "") changes.name = name;
        if (password !== "") changes.password = password;
        if (findPeer(addr)) {
            updatePeer(addr, changes);
        } else {
            var list = home.peers.slice();
            list.push(Object.assign({ address: addr, name: "", password: "", favorite: false, os: "" }, changes));
            home.peers = list;
            savePeers();
        }
        home.peerTab = "recent";
        return "";
    }

    // "Add computer" with a connector type (`sourceId` ""), or "Edit" of
    // one. The fields go to the app's helper; the card keeps the name and
    // the type. Returns an error message or "". A type with a set-up (a
    // pairing) is not added yet: the dialog shows what it wants shown
    // (addDialog.setupShown) and the card comes when it is done.
    // `group` says which computer it is (see groupFor); left out: nothing said.
    function saveSource(connectorId, sourceId, name, values, group) {
        var said = groupFor(group);
        if (said.error !== "") return said.error;
        var answer;
        try {
            answer = JSON.parse(sourceId === ""
                ? bridge.bridgeAddSourceGrouped(name, connectorId, JSON.stringify(values), said.group)
                : bridge.bridgeUpdateSourceGrouped(sourceId, name, JSON.stringify(values), said.group));
        } catch (e) {
            return I18n.t("The computer could not be saved");
        }
        if (answer && !answer.error && answer.setup && sourceId === "") {
            addDialog.setupShow(answer.setup, "", answer.message, answer.code);
            return "";
        }
        if (!answer || answer.error || !answer.id) return I18n.tError(String((answer && answer.error) || "The computer could not be saved"));
        if (sourceId === "") {
            sourceAdded(answer.id, name, connectorId);
        } else {
            refreshOwnComputers();
            updatePeer("source:" + sourceId, { name: name });
        }
        return "";
    }

    // A computer of another type was added (at once, or when its set-up was
    // done): its card.
    function sourceAdded(id, name, connectorId) {
        refreshOwnComputers();
        var list = home.peers.slice();
        list.push({ address: "source:" + id, name: name, type: connectorId, sourceId: id, favorite: false, lastUsed: Date.now() });
        home.peers = list;
        savePeers();
        home.peerTab = "recent";
    }

    // How the set-up of the computer being added stands (asked once a
    // second by the dialog).
    function askSourceSetup(setup) {
        var state;
        try { state = JSON.parse(bridge.bridgeSourceSetup(setup)); } catch (e) { state = null; }
        if (addDialog.setupId !== setup) return;
        if (!state || state.state === "failed") {
            addDialog.setupFailed(I18n.tError(String((state && state.error) || "")));
        } else if (state.state === "done" && state.id) {
            var name = addDialog.setupName;
            var type = addDialog.setupType;
            addDialog.setupFinished();
            sourceAdded(state.id, name, type);
            addDialog.close();
        } else {
            addDialog.setupUpdate(state.message, state.code);
        }
    }

    function editSource(peer) {
        refreshOwnComputers();
        var s = ownSource(peer.sourceId || "");
        var same = sameOptionsFor(peer);
        addDialog.openEdit({
            id: peer.sourceId, name: peer.name || (s ? s.name : ""),
            connector: peer.type || (s ? s.connector : ""), connector_name: typeName(peer)
        }, s ? s.values : ({}), same.options, same.now);
    }

    // Removes the card and what the helper keeps for it (its address and password).
    function removeSourcePeer(peer) {
        var error = bridge.bridgeRemoveSource(peer.sourceId || "");
        if (error !== "") { setStatus(I18n.tError(error), true); return; }
        removePeer(peer.address);
        refreshOwnComputers();
    }

    // One click connects: the app's helper has the password.
    function connectSource(peer) {
        updatePeer(peer.address, { lastUsed: Date.now() });
        var name = peer.name || I18n.t(typeName(peer));
        setStatus(I18n.t("Connecting to %1…").arg(name), false);
        var s = streamSettingsFor(peer);
        home.sessionPeer = peer.address;
        home.sessionSource = peer.sourceId || "";
        home.startSessionRequested(
            "bridge:", "", "direct", name, 0,
            s.res, s.fps, s.codec, s.bitrate, s.queueLimit,
            s.renderBackend === "software", s.renderBackend,
            s.inputProtocol, s.encoder, s.displayId, peer.sourceId || "");
    }

    // From main.qml: the displays of the source being shown, for the
    // display choice in that computer's stream settings.
    function sourceDisplaysSeen(displaysJson, current) {
        var list = [];
        try { list = JSON.parse(displaysJson) || []; } catch (e) { return; }
        if (home.sessionPeer === "") { serverDashboard.sourceDisplaysSeen(list); return; }
        if (home.sessionSource === "") return;
        updatePeer(home.sessionPeer, { sourceDisplays: { source: home.sessionSource, list: list } });
    }

    // The displays remembered for a card, as the stream settings take them:
    // { "<source id>": [{ id, name, width, height }] }.
    function seenDisplays(peer) {
        var out = {};
        if (peer && peer.sourceDisplays && peer.sourceDisplays.source)
            out[peer.sourceDisplays.source] = peer.sourceDisplays.list || [];
        return out;
    }

    function setPref(key, value) {
        var next = Object.assign({}, home.prefs);
        next[key] = value;
        home.prefs = next;
        if (key === "fps") home.fpsFollowsScreen = false;
        if (key === "theme") Theme.mode = value;
        if (key === "language") I18n.lang = value;
        bridge.saveStore("app_settings", JSON.stringify(storedPrefs()));
    }

    function savePeers() {
        bridge.saveStore("peers", JSON.stringify({ peers: home.peers }));
    }

    function refreshHost() {
        try { home.hostStatus = JSON.parse(bridge.hostStatus()); } catch (e) { console.log("host status: " + e); }
        refreshServer();
        // Remote control on and signed in: make sure this computer is in
        // that server's list (once per server and sign-in).
        if (!home.hostStatus.enabled) home.agentSyncedFor = "";
        else if (serverDashboard.isLoggedIn && !home.pendingAgentSync
                 && home.agentSyncedFor !== agentSyncKey()) syncAgentServer();
    }

    function refreshServer() {
        try { home.serverStatus = JSON.parse(bridge.serverStatus()); } catch (e) { console.log("server status: " + e); }
        if (home.serverStatus.running) home.serverBusy = false;
    }

    // "address" or "address:port" when the port isn't the default.
    function withPort(ip) {
        return ip + (home.hostStatus.port === 8091 ? "" : ":" + home.hostStatus.port);
    }

    function hostAddress(i) {
        var list = home.hostStatus.addresses;
        return list.length > i ? withPort(list[i]) : "—";
    }

    function setServer(on) {
        home.serverError = on ? bridge.serverStart() : bridge.serverStop();
        home.serverBusy = on && home.serverError === "";
        refreshServer();
        serverRefreshSoon.restart();
    }

    function setServerPort(port) {
        var err = bridge.serverSetPort(port);
        home.serverError = err;
        refreshServer();
        serverRefreshSoon.restart();
        return err;
    }

    // The web console: the server the app is signed in to, else the app's
    // own server.
    function webConsoleUrl() {
        if (serverDashboard.isLoggedIn) return serverDashboard.serverUrl;
        if (home.serverStatus.running) return home.serverStatus.local_url;
        return "";
    }

    function sameServer(a, b) {
        function norm(u) { return String(u || "").trim().replace(/\/+$/, "").replace("://localhost", "://127.0.0.1").toLowerCase(); }
        return norm(a) !== "" && norm(a) === norm(b);
    }

    // Opens the web console signed in as the app's user: the session goes
    // in the URL fragment, which the browser never sends to the server.
    property bool pendingWebConsole: false
    function openWebConsole(url) {
        var target = url || webConsoleUrl();
        if (target === "") return;
        var d = serverDashboard;
        if (d.isLoggedIn && d.token !== "" && sameServer(d.serverUrl, target)) {
            Qt.openUrlExternally(target.replace(/\/+$/, "") + "/#session=" + encodeURIComponent(d.token)
                                 + "&user=" + encodeURIComponent(d.username));
            return;
        }
        // Not signed in anywhere: sign in to this computer's server first.
        if (!d.isLoggedIn && home.serverStatus.running && sameServer(target, home.serverStatus.local_url)
                && home.serverStatus.admin_password !== "") {
            home.pendingWebConsole = true;
            signInLocal();
            return;
        }
        Qt.openUrlExternally(target);
    }

    Connections {
        target: bridge
        function onLoginResult(success, errorMsg, tok, user, srv) {
            if (!home.pendingWebConsole) return;
            home.pendingWebConsole = false;
            if (success)
                Qt.openUrlExternally(String(srv).replace(/\/+$/, "") + "/#session=" + encodeURIComponent(tok) + "&user=" + encodeURIComponent(user));
            else
                Qt.openUrlExternally(home.serverStatus.local_url);
        }
    }

    // ---- This computer in the signed-in server's list ----
    // While remote control is on, this computer's agent registers with the
    // server the app is signed in to (unless it already uses another one).
    property bool pendingAgentSync: false
    property string hostServerNote: ""
    // Server and sign-in this computer was last registered for.
    property string agentSyncedFor: ""

    function agentSyncKey() { return serverDashboard.serverUrl + "|" + serverDashboard.token; }

    function syncAgentServer() {
        if (!home.hostStatus.enabled || !serverDashboard.isLoggedIn || serverDashboard.token === "") return;
        home.pendingAgentSync = true;
        home.agentSyncKeyPending = agentSyncKey();
        bridge.fetchAgentToken(serverDashboard.serverUrl, serverDashboard.token);
    }
    property string agentSyncKeyPending: ""

    Connections {
        target: bridge
        function onLoginResult(success, errorMsg, tok, user, srv) { if (success) agentSyncSoon.restart(); }
        function onCredentialsLoaded(success, srv, tok, user) { if (success) agentSyncSoon.restart(); }
        function onAgentTokenResult(success, errorMsg, tok) {
            if (!home.pendingAgentSync) return;
            home.pendingAgentSync = false;
            // Tried for this sign-in: don't retry every refresh.
            home.agentSyncedFor = home.agentSyncKeyPending;
            if (!success) {
                home.hostServerNote = I18n.t("Couldn't add this computer to the server: %1").arg(errorMsg);
                return;
            }
            var result = String(bridge.hostUseServer(serverDashboard.serverUrl, tok));
            if (result.indexOf("elsewhere ") === 0) {
                home.hostServerNote = I18n.t("Listed on another server (%1), not on the one signed in to.").arg(result.slice(10));
            } else if (result.indexOf("error: ") === 0) {
                home.hostServerNote = I18n.t("Couldn't add this computer to the server: %1").arg(result.slice(7));
            } else {
                home.hostServerNote = "";
                hostsRefreshSoon.restart();
            }
        }
    }

    Timer {
        // The dashboard's own state updates first on sign-in.
        id: agentSyncSoon
        interval: 300
        onTriggered: home.syncAgentServer()
    }

    Timer {
        // The agent needs a moment to connect before it is listed.
        id: hostsRefreshSoon
        interval: 3500
        onTriggered: if (serverDashboard.isLoggedIn) bridge.fetchHosts()
    }

    // Sign the app in to its own server with the first admin account.
    function signInLocal() {
        if (home.serverStatus.admin_password === "") return;
        serverDashboard.authLoading = true;
        bridge.login(home.serverStatus.local_url, "admin", home.serverStatus.admin_password);
    }

    function setRemoteControl(on) {
        var err = on ? bridge.hostStart() : bridge.hostStop();
        home.hostError = err;
        if (!on) hostsRefreshSoon.restart();
        // The agent needs a moment to open its port.
        refreshHost();
        hostRefreshSoon.restart();
    }

    function setHostPassword(password) {
        var err = bridge.hostSetPassword(password);
        home.hostError = err;
        refreshHost();
        return err;
    }

    function setHostPort(port) {
        var err = bridge.hostSetPort(port);
        home.hostError = err;
        refreshHost();
        hostRefreshSoon.restart();
        return err;
    }

    // "host" or "host:port" (default port 8091), without scheme or path.
    function normalizeAddress(text) {
        var addr = String(text || "").trim()
            .replace(/^wss?:\/\//, "").replace(/^https?:\/\//, "").replace(/\/.*$/, "");
        if (addr.length === 0) return "";
        var hasPort = addr.charAt(0) === "[" ? addr.indexOf("]:") !== -1 : addr.indexOf(":") !== -1;
        return hasPort ? addr : addr + ":8091";
    }

    function displayAddress(addr) {
        return addr.endsWith(":8091") ? addr.slice(0, -5) : addr;
    }

    function findPeer(address) {
        for (var i = 0; i < home.peers.length; i++) {
            if (home.peers[i].address === address) return home.peers[i];
        }
        return null;
    }

    function updatePeer(address, changes) {
        var list = home.peers.slice();
        for (var i = 0; i < list.length; i++) {
            if (list[i].address === address) {
                list[i] = Object.assign({}, list[i], changes);
                home.peers = list;
                savePeers();
                return;
            }
        }
    }

    function rememberPeer(address, password, remember) {
        var existing = findPeer(address);
        var changes = { lastUsed: Date.now() };
        if (remember) changes.password = password;
        if (existing) {
            updatePeer(address, changes);
        } else {
            var list = home.peers.slice();
            list.push(Object.assign({ address: address, name: "", password: "", favorite: false, os: "" }, changes));
            home.peers = list;
            savePeers();
        }
    }

    function removePeer(address) {
        home.peers = home.peers.filter(function (p) { return p.address !== address; });
        savePeers();
    }

    // The cards of the list: by tab and search, the one used last first.
    function visiblePeers() {
        var q = home.searchText.trim().toLowerCase();
        var list = peerCards().filter(function (card) {
            if (home.peerTab === "favorites" && !card.favorite) return false;
            if (q === "") return true;
            return card.peers.some(function (p) { return (p.address + " " + (p.name || "")).toLowerCase().indexOf(q) !== -1; })
                || String(card.title).toLowerCase().indexOf(q) !== -1;
        });
        list.sort(function (a, b) { return (b.lastUsed || 0) - (a.lastUsed || 0); });
        return list;
    }

    // Connect to a computer by address; asks for the password unless saved.
    function requestConnect(text) {
        var known = findPeer(String(text || ""));
        if (isSource(known)) { connectSource(known); return; }
        var addr = normalizeAddress(text);
        if (addr === "") {
            setStatus(I18n.t("Enter the address of the computer to control"), true);
            addressInput.forceActiveFocus();
            return;
        }
        var peer = findPeer(addr);
        if (peer && peer.password) {
            connectTo(addr, peer.password, true);
        } else {
            passwordDialog.openFor(addr);
        }
    }

    function connectTo(addr, password, remember) {
        rememberPeer(addr, password, remember);
        var peer = findPeer(addr);
        var name = (peer && peer.name) ? peer.name : displayAddress(addr);
        setStatus(I18n.t("Connecting to %1…").arg(name), false);
        bridge.saveDirectTarget(addr, remember ? password : "");
        var s = streamSettingsFor(peer);
        home.sessionPeer = addr;
        home.sessionSource = s.sourceId || "";
        home.startSessionRequested(
            "ws://" + addr, password, "direct", name, 0,
            s.res, s.fps, s.codec, s.bitrate, s.queueLimit,
            s.renderBackend === "software", s.renderBackend,
            s.inputProtocol, s.encoder, s.displayId, s.sourceId);
    }

    // Stream settings for a saved computer: its own (gear on the card) or
    // the defaults from Settings > Display.
    function streamSettingsFor(peer) {
        applyScreenFps();
        var d = {
            res: home.prefs.res, fps: home.prefs.fps, codec: home.prefs.codec,
            bitrate: home.prefs.bitrate, queueLimit: 256, renderBackend: home.prefs.renderBackend,
            inputProtocol: "webrtc", encoder: "auto", displayId: "default", sourceId: ""
        };
        return Object.assign(d, (peer && peer.settings) || {});
    }

    // Files / stream settings of a saved computer need its password: ask
    // for it first when it isn't saved.
    function withPassword(address, action) {
        var peer = findPeer(address);
        if (peer && peer.password) action(peer.password);
        else passwordDialog.openFor(address, action);
    }

    function openPeerFiles(address) {
        // A computer of another type: its files through its connector, by
        // the app's own helper (which knows its address and password).
        var source = findPeer(address);
        if (isSource(source)) {
            fileManager.openFor("bridge:", "", "direct", source.name || "", source.sourceId || "");
            return;
        }
        withPassword(address, function (password) {
            var peer = findPeer(address);
            fileManager.openFor("ws://" + address, password, "direct",
                (peer && peer.name) ? peer.name : displayAddress(address));
        });
    }

    function openPeerSettings(address) {
        var known = findPeer(address);
        if (isSource(known)) {
            // No password to ask for and nothing to ask the computer: its
            // displays are the ones its last session listed.
            var own = streamSettingsFor(known);
            peerSettings.address = address;
            peerSettings.password = "";
            peerSettings.hostName = known.name || I18n.t(typeName(known));
            peerSettings.sourceOnly = true;
            peerSettings.sourceDisplays = seenDisplays(known);
            peerSettings.setCurrentSettings(own.res, own.fps, own.codec, own.bitrate, own.queueLimit,
                own.renderBackend === "software", own.inputProtocol, own.encoder, own.displayId,
                own.renderBackend, known.sourceId || "");
            peerSettings.open();
            return;
        }
        withPassword(address, function (password) {
            var peer = findPeer(address);
            var s = streamSettingsFor(peer);
            peerSettings.address = address;
            peerSettings.password = password;
            peerSettings.hostName = (peer && peer.name) ? peer.name : displayAddress(address);
            peerSettings.sourceOnly = false;
            peerSettings.sourceDisplays = seenDisplays(peer);
            peerSettings.setCurrentSettings(s.res, s.fps, s.codec, s.bitrate, s.queueLimit,
                s.renderBackend === "software", s.inputProtocol, s.encoder, s.displayId,
                s.renderBackend, s.sourceId);
            bridge.fetchCapabilities("ws://" + address, password, "direct");
            peerSettings.open();
        });
    }

    // From main.qml: what the computer whose settings are open can do.
    function peerCapabilities(displaysJson, encodersJson, hostOs, sourcesJson) {
        if (!peerSettings.visible || peerSettings.address === "") return;
        peerSettings.updateCapabilities(displaysJson, encodersJson, sourcesJson);
        if (hostOs) updatePeer(peerSettings.address, { os: hostOs });
    }

    // Online status of the saved computers (a TCP check of their port).
    property int onlineTick: 0
    function peerOnline(address) {
        onlineTick; // re-evaluate when the checks come back
        return bridge.peerOnline(address);
    }
    Timer {
        interval: 10000
        running: home.visible && home.peerTab !== "server"
        repeat: true
        triggeredOnStart: true
        onTriggered: {
            // Only Lunaris computers have a port of their own to check.
            for (var i = 0; i < home.peers.length; i++)
                if (!home.isSource(home.peers[i])) bridge.probePeer(home.peers[i].address);
            onlineRefresh.restart();
        }
    }
    Timer { id: onlineRefresh; interval: 1700; onTriggered: home.onlineTick++ }

    function copyText(text) { clipboardHelper.copy(text); }

    function setStatus(text, isError) {
        home.statusText = text;
        home.statusIsError = isError;
    }

    // A stream ended with an error (main.qml).
    function showError(message) {
        setStatus(message, true);
        home.page = "home";
    }

    Component.onCompleted: loadAll()

    Timer {
        // Pick up changes made elsewhere (the agent starting, a new address).
        interval: 4000
        running: home.visible
        repeat: true
        onTriggered: home.refreshHost()
    }

    Timer {
        id: hostRefreshSoon
        interval: 1200
        onTriggered: home.refreshHost()
    }

    Timer {
        // The server needs a few seconds to create its database and open the port.
        id: serverRefreshSoon
        interval: 1000
        repeat: true
        property int left: 8
        onTriggered: {
            home.refreshServer();
            if (home.serverStatus.running || --left <= 0) { stop(); left = 8; }
        }
    }

    // ---------------------------------------------------------------
    // Small building blocks
    // ---------------------------------------------------------------
    component TopTab: AbstractButton {
        id: tt
        property string iconName: ""
        property bool current: false
        hoverEnabled: true
        implicitHeight: 34
        implicitWidth: ttRow.implicitWidth + 24
        background: Rectangle {
            radius: Theme.radiusSm
            color: tt.current ? Theme.surface2 : (tt.hovered ? Theme.ink(0.04) : "transparent")
            border.color: tt.current ? Theme.border : "transparent"
        }
        contentItem: Item {
            Row {
                id: ttRow
                anchors.centerIn: parent
                spacing: 8
                Icon { name: tt.iconName; size: 15; color: tt.current ? Theme.accentFg : Theme.textMuted; anchors.verticalCenter: parent.verticalCenter }
                Text { text: tt.text; color: tt.current ? Theme.textStrong : Theme.textMuted; font.pixelSize: 13; font.weight: Font.Medium; anchors.verticalCenter: parent.verticalCenter }
            }
        }
    }

    component PeerTab: AbstractButton {
        id: ptab
        property string iconName: ""
        property bool current: false
        hoverEnabled: true
        implicitHeight: 36
        implicitWidth: ptRow.implicitWidth + 16
        ToolTip.visible: hovered
        ToolTip.text: text
        ToolTip.delay: 600
        background: Item {
            Rectangle {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.bottom: parent.bottom
                height: 2
                radius: 1
                color: ptab.current ? Theme.accentFg : "transparent"
            }
        }
        contentItem: Item {
            Row {
                id: ptRow
                anchors.centerIn: parent
                spacing: 7
                Icon { name: ptab.iconName; size: 16; color: ptab.current ? Theme.textStrong : (ptab.hovered ? Theme.text : Theme.textMuted); anchors.verticalCenter: parent.verticalCenter }
                Text { text: ptab.text; color: ptab.current ? Theme.textStrong : (ptab.hovered ? Theme.text : Theme.textMuted); font.pixelSize: 13; font.weight: ptab.current ? Font.DemiBold : Font.Normal; anchors.verticalCenter: parent.verticalCenter }
            }
        }
    }

    // ---------------------------------------------------------------
    // Layout: tabs on top, page below, status bar at the bottom
    // ---------------------------------------------------------------
    Rectangle {
        id: topBar
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        height: 48
        color: Theme.surface
        Rectangle { anchors.left: parent.left; anchors.right: parent.right; anchors.bottom: parent.bottom; height: 1; color: Theme.border }

        Row {
            anchors.left: parent.left
            anchors.leftMargin: 14
            anchors.verticalCenter: parent.verticalCenter
            spacing: 6

            Image {
                source: "icon.png"
                width: 22; height: 22
                sourceSize.width: 44; sourceSize.height: 44
                smooth: true
                anchors.verticalCenter: parent.verticalCenter
            }
            Item { width: 8; height: 1 }
            TopTab { text: I18n.t("Home"); iconName: "home"; current: home.page === "home"; onClicked: home.page = "home" }
            TopTab { text: I18n.t("Settings"); iconName: "settings"; current: home.page === "settings"; onClicked: home.page = "settings" }
        }

        // Right: the server (if any) and its web console
        Row {
            anchors.right: parent.right
            anchors.rightMargin: 14
            anchors.verticalCenter: parent.verticalCenter
            spacing: 10

            AbstractButton {
                id: serverChip
                anchors.verticalCenter: parent.verticalCenter
                hoverEnabled: true
                implicitHeight: 30
                implicitWidth: chipRow.implicitWidth + 20
                onClicked: { home.page = "home"; home.peerTab = "server"; }
                ToolTip.visible: hovered
                ToolTip.delay: 400
                ToolTip.text: home.serverStatus.running ? I18n.t("Server running at %1").arg(home.serverStatus.lan_url)
                    : (serverDashboard.isLoggedIn ? I18n.t("Signed in to %1").arg(serverDashboard.serverUrl) : I18n.t("No server: start one on this computer or sign in to one"))
                background: Rectangle {
                    radius: 15
                    color: serverChip.hovered ? Theme.surface2 : "transparent"
                    border.color: Theme.border
                }
                contentItem: Item {
                    Row {
                        id: chipRow
                        anchors.centerIn: parent
                        spacing: 7
                        Rectangle {
                            width: 7; height: 7; radius: 3.5
                            anchors.verticalCenter: parent.verticalCenter
                            color: home.serverStatus.running || serverDashboard.isLoggedIn ? Theme.success : Theme.textSubtle
                        }
                        Text {
                            anchors.verticalCenter: parent.verticalCenter
                            text: home.serverStatus.running ? I18n.t("Server on this computer")
                                : (serverDashboard.isLoggedIn ? I18n.t("Signed in to a server") : I18n.t("No server"))
                            color: Theme.textMuted
                            font.pixelSize: 12
                        }
                    }
                }
            }

            PrimaryButton {
                anchors.verticalCenter: parent.verticalCenter
                visible: home.webConsoleUrl() !== ""
                implicitHeight: 32
                leftPadding: 14
                rightPadding: 14
                text: I18n.t("Web console")
                ToolTip.visible: hovered
                ToolTip.delay: 400
                ToolTip.text: I18n.t("Manage users, groups and computers in your browser (%1)").arg(home.webConsoleUrl())
                onClicked: home.openWebConsole()
            }
        }
    }

    Rectangle {
        id: statusBar
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        height: 32
        color: Theme.surface
        Rectangle { anchors.left: parent.left; anchors.right: parent.right; anchors.top: parent.top; height: 1; color: Theme.border }

        Row {
            anchors.left: parent.left
            anchors.leftMargin: 16
            anchors.verticalCenter: parent.verticalCenter
            spacing: 8
            Rectangle {
                width: 8; height: 8; radius: 4
                anchors.verticalCenter: parent.verticalCenter
                color: home.statusIsError ? Theme.danger : (home.hostStatus.running ? Theme.success : Theme.textSubtle)
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                color: home.statusIsError ? Theme.danger : Theme.textMuted
                font.pixelSize: 12
                elide: Text.ElideRight
                width: home.width - 220
                // (It can hold what another computer said: never markup.)
                textFormat: Text.PlainText
                text: home.statusText !== "" ? home.statusText
                    : (home.hostStatus.running ? I18n.t("Ready. Others can control this computer with its address and password.")
                                               : I18n.t("Ready. Remote control of this computer is off."))
            }
        }
        Text {
            anchors.right: parent.right
            anchors.rightMargin: 16
            anchors.verticalCenter: parent.verticalCenter
            text: "Lunaris " + bridge.appVersion()
            color: Theme.textSubtle
            font.pixelSize: 11
        }
    }

    Item {
        id: pageArea
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: topBar.bottom
        anchors.bottom: statusBar.top

        // ===========================================================
        // HOME
        // ===========================================================
        Item {
            anchors.fill: parent
            visible: home.page === "home"

            // ---------------- This computer ----------------
            Rectangle {
                id: sidePanel
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.bottom: parent.bottom
                width: 300
                color: Theme.surface
                Rectangle { anchors.right: parent.right; anchors.top: parent.top; anchors.bottom: parent.bottom; width: 1; color: Theme.border }

                Flickable {
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.bottom: sideFooter.top
                    anchors.rightMargin: 1
                    contentHeight: sideColumn.implicitHeight + 40
                    clip: true
                    boundsBehavior: Flickable.StopAtBounds

                    ColumnLayout {
                        id: sideColumn
                        x: 22
                        y: 22
                        width: sidePanel.width - 44
                        spacing: 18

                        ColumnLayout {
                            spacing: 4
                            Layout.fillWidth: true
                            Text { text: I18n.t("This computer"); color: Theme.textStrong; font.pixelSize: 19; font.weight: Font.DemiBold }
                            Text {
                                Layout.fillWidth: true
                                text: I18n.t("Others can control it with this address and password.")
                                color: Theme.textMuted
                                font.pixelSize: 12
                                wrapMode: Text.WordWrap
                            }
                        }

                        // On/off
                        Rectangle {
                            Layout.fillWidth: true
                            implicitHeight: 52
                            radius: Theme.radius
                            color: home.hostStatus.running ? Qt.rgba(34 / 255, 197 / 255, 94 / 255, 0.08) : Theme.surface2
                            border.color: home.hostStatus.running ? Qt.rgba(34 / 255, 197 / 255, 94 / 255, 0.30) : Theme.border
                            RowLayout {
                                anchors.fill: parent
                                anchors.leftMargin: 14
                                anchors.rightMargin: 8
                                spacing: 10
                                Icon { name: "power"; size: 17; color: home.hostStatus.running ? Theme.success : Theme.textMuted }
                                ColumnLayout {
                                    spacing: 1
                                    Layout.fillWidth: true
                                    Text { text: I18n.t("Allow remote control"); color: Theme.textStrong; font.pixelSize: 13; font.weight: Font.Medium; Layout.fillWidth: true; elide: Text.ElideRight }
                                    Text {
                                        text: home.hostStatus.running ? I18n.t("On") : (home.hostStatus.enabled ? I18n.t("Starting…") : I18n.t("Off"))
                                        color: home.hostStatus.running ? Theme.success : Theme.textSubtle
                                        font.pixelSize: 11
                                    }
                                }
                                Toggle {
                                    id: remoteSwitch
                                    checked: home.hostStatus.enabled
                                    onToggled: {
                                        home.setRemoteControl(checked);
                                        checked = Qt.binding(function () { return home.hostStatus.enabled; });
                                    }
                                    Accessible.name: "Allow remote control"
                                }
                            }
                        }

                        // Address and password
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 14
                            opacity: home.hostStatus.enabled ? 1 : 0.5

                            ColumnLayout {
                                Layout.fillWidth: true
                                spacing: 2
                                Text { text: I18n.t("Address"); color: Theme.textMuted; font.pixelSize: 12; font.weight: Font.Medium }
                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: 2
                                    Text {
                                        Layout.fillWidth: true
                                        text: home.hostAddress(0)
                                        color: Theme.textStrong
                                        font.pixelSize: 24
                                        font.weight: Font.DemiBold
                                        elide: Text.ElideRight
                                    }
                                    IconButton {
                                        iconName: "copy"
                                        tip: I18n.t("Copy address")
                                        visible: home.hostStatus.addresses.length > 0
                                        onClicked: { clipboardHelper.copy(home.hostAddress(0)); home.setStatus(I18n.t("Address copied"), false); }
                                    }
                                }
                                Text {
                                    visible: home.hostStatus.hostname !== ""
                                    text: home.hostStatus.hostname
                                    color: Theme.textSubtle
                                    font.pixelSize: 11
                                }
                                // Other addresses (VPN, other networks), folded away
                                Text {
                                    visible: home.hostStatus.addresses.length > 1
                                    Layout.topMargin: 4
                                    text: home.showAllAddresses ? I18n.t("Hide other addresses") : I18n.t(home.hostStatus.addresses.length > 2 ? "+%1 other addresses" : "+%1 other address").arg(home.hostStatus.addresses.length - 1)
                                    color: moreAddrMa.containsMouse ? Theme.accentFg : Theme.textMuted
                                    font.pixelSize: 12
                                    MouseArea {
                                        id: moreAddrMa
                                        anchors.fill: parent
                                        hoverEnabled: true
                                        cursorShape: Qt.PointingHandCursor
                                        onClicked: home.showAllAddresses = !home.showAllAddresses
                                    }
                                }
                                Repeater {
                                    model: home.showAllAddresses ? home.hostStatus.addresses.slice(1) : []
                                    delegate: RowLayout {
                                        Layout.fillWidth: true
                                        spacing: 2
                                        readonly property string full: home.withPort(modelData)
                                        Text { Layout.fillWidth: true; text: parent.full; color: Theme.text; font.pixelSize: 13; elide: Text.ElideRight }
                                        IconButton {
                                            iconName: "copy"; iconSize: 14; implicitWidth: 26; implicitHeight: 26
                                            tip: I18n.t("Copy address")
                                            onClicked: { clipboardHelper.copy(parent.full); home.setStatus(I18n.t("Address copied"), false); }
                                        }
                                    }
                                }
                                Text {
                                    visible: home.hostStatus.addresses.length === 0
                                    text: I18n.t("No network connection")
                                    color: Theme.textSubtle
                                    font.pixelSize: 13
                                }
                            }

                            ColumnLayout {
                                Layout.fillWidth: true
                                spacing: 2
                                Text { text: I18n.t("Password"); color: Theme.textMuted; font.pixelSize: 12; font.weight: Font.Medium }
                                Text {
                                    Layout.fillWidth: true
                                    text: home.passwordVisible ? home.hostStatus.password : home.hostStatus.password.replace(/./g, "•")
                                    color: Theme.textStrong
                                    font.pixelSize: home.passwordVisible ? 18 : 16
                                    font.weight: Font.Medium
                                    font.family: home.passwordVisible ? "monospace" : Qt.application.font.family
                                    elide: Text.ElideRight
                                }
                                Row {
                                    spacing: 2
                                    Layout.leftMargin: -6
                                    IconButton {
                                        iconName: home.passwordVisible ? "eyeOff" : "eye"
                                        tip: home.passwordVisible ? I18n.t("Hide password") : I18n.t("Show password")
                                        onClicked: home.passwordVisible = !home.passwordVisible
                                    }
                                    IconButton {
                                        iconName: "copy"
                                        tip: I18n.t("Copy password")
                                        onClicked: { clipboardHelper.copy(home.hostStatus.password); home.setStatus(I18n.t("Password copied"), false); }
                                    }
                                    IconButton {
                                        iconName: "refresh"
                                        tip: I18n.t("New random password")
                                        onClicked: {
                                            if (home.setHostPassword("") === "") {
                                                home.passwordVisible = true;
                                                home.setStatus(I18n.t("New password set. Computers using the old one can no longer connect."), false);
                                            }
                                        }
                                    }
                                    IconButton {
                                        iconName: "edit"
                                        tip: I18n.t("Set your own password")
                                        onClicked: customPasswordDialog.open()
                                    }
                                }
                            }
                        }

                        Text {
                            Layout.fillWidth: true
                            visible: home.hostError !== "" || home.hostStatus.error !== ""
                            text: home.hostError !== "" ? home.hostError : home.hostStatus.error
                            color: Theme.danger
                            font.pixelSize: 12
                            wrapMode: Text.WordWrap
                        }

                        Text {
                            Layout.fillWidth: true
                            visible: home.hostStatus.enabled && home.hostServerNote !== ""
                            text: home.hostServerNote
                            color: Theme.textMuted
                            font.pixelSize: 11
                            wrapMode: Text.WordWrap
                        }

                        // Remote keyboard/mouse need /dev/uinput (Linux).
                        Rectangle {
                            Layout.fillWidth: true
                            visible: home.hostStatus.enabled && home.hostStatus.input_ready === false
                            implicitHeight: inputNote.implicitHeight + 24
                            radius: Theme.radius
                            color: Qt.rgba(245 / 255, 158 / 255, 11 / 255, 0.10)
                            border.color: Qt.rgba(245 / 255, 158 / 255, 11 / 255, 0.35)
                            Text {
                                id: inputNote
                                anchors.fill: parent
                                anchors.margins: 12
                                text: I18n.t("The other computer will see this screen but can't use the keyboard and mouse yet: Lunaris has no access to /dev/uinput. Install the Lunaris package, or run the AppImage once with --setup-host, then sign out and in again.")
                                color: Theme.text
                                font.pixelSize: 11
                                wrapMode: Text.WordWrap
                                lineHeight: 1.2
                            }
                        }
                    }
                }

                // Footer: where direct connections are meant to be used
                Item {
                    id: sideFooter
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.bottom: parent.bottom
                    anchors.rightMargin: 1
                    height: footerRow.implicitHeight + 24
                    Rectangle { anchors.left: parent.left; anchors.right: parent.right; anchors.top: parent.top; height: 1; color: Theme.border }
                    RowLayout {
                        id: footerRow
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.leftMargin: 22
                        anchors.rightMargin: 22
                        spacing: 8
                        Icon { name: "shield"; size: 14; color: Theme.textSubtle; Layout.alignment: Qt.AlignTop }
                        Text {
                            Layout.fillWidth: true
                            text: I18n.t("Direct connections: for your network or a VPN. Video and input are encrypted.")
                            color: Theme.textSubtle
                            font.pixelSize: 11
                            wrapMode: Text.WordWrap
                        }
                    }
                }
            }

            // ---------------- Control a remote computer ----------------
            Item {
                id: mainArea
                anchors.left: sidePanel.right
                anchors.right: parent.right
                anchors.top: parent.top
                anchors.bottom: parent.bottom

                Rectangle {
                    id: connectCard
                    anchors.left: parent.left
                    anchors.top: parent.top
                    anchors.margins: 24
                    anchors.right: parent.right
                    // The Server tab needs the room (and has its own list).
                    visible: home.peerTab !== "server"
                    height: visible ? connectColumn.implicitHeight + 40 : 0
                    radius: Theme.radiusLg
                    color: Theme.surface
                    border.color: Theme.border

                    ColumnLayout {
                        id: connectColumn
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.top: parent.top
                        anchors.margins: 20
                        spacing: 14

                        Text { text: I18n.t("Control a remote computer"); color: Theme.textStrong; font.pixelSize: 17; font.weight: Font.DemiBold }

                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 10
                            Rectangle {
                                Layout.fillWidth: true
                                implicitHeight: 42
                                radius: Theme.radiusSm
                                color: Theme.bg
                                border.color: addressInput.activeFocus ? Theme.accent : Theme.borderStrong
                                border.width: 1
                                TextInput {
                                    id: addressInput
                                    anchors.fill: parent
                                    anchors.leftMargin: 14
                                    anchors.rightMargin: 14
                                    verticalAlignment: TextInput.AlignVCenter
                                    color: Theme.textStrong
                                    selectionColor: Theme.accentA(0.4)
                                    font.pixelSize: 17
                                    clip: true
                                    onAccepted: home.requestConnect(text)
                                    Accessible.name: "Computer address"
                                    Text {
                                        anchors.verticalCenter: parent.verticalCenter
                                        visible: addressInput.text.length === 0
                                        text: I18n.t("Address, e.g. 192.168.1.20")
                                        color: Theme.textSubtle
                                        font.pixelSize: 15
                                    }
                                }
                            }
                            PrimaryButton {
                                text: I18n.t("Connect")
                                implicitHeight: 42
                                onClicked: home.requestConnect(addressInput.text)
                            }
                        }
                        Text {
                            Layout.fillWidth: true
                            text: I18n.t("Enter the address shown under \"This computer\" on the other computer. Computers on a Lunaris server are under Server below.")
                            color: Theme.textSubtle
                            font.pixelSize: 11
                            wrapMode: Text.WordWrap
                        }
                    }
                }

                // Tabs: recent / favorites / server, search on the right
                Item {
                    id: peerBar
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: connectCard.visible ? connectCard.bottom : parent.top
                    anchors.topMargin: connectCard.visible ? 18 : 12
                    anchors.leftMargin: 24
                    anchors.rightMargin: 24
                    height: 38
                    Rectangle { anchors.left: parent.left; anchors.right: parent.right; anchors.bottom: parent.bottom; height: 1; color: Theme.border }

                    Row {
                        anchors.left: parent.left
                        anchors.bottom: parent.bottom
                        spacing: 6
                        PeerTab { text: I18n.t("Recent"); iconName: "clock"; current: home.peerTab === "recent"; onClicked: home.peerTab = "recent" }
                        PeerTab { text: I18n.t("Favorites"); iconName: "star"; current: home.peerTab === "favorites"; onClicked: home.peerTab = "favorites" }
                        PeerTab { text: I18n.t("Server"); iconName: "server"; current: home.peerTab === "server"; onClicked: home.peerTab = "server" }
                    }

                    // Add a computer of any type without connecting to it.
                    PrimaryButton {
                        id: addComputerButton
                        objectName: "addComputerButton"
                        visible: home.peerTab !== "server"
                        anchors.right: searchBox.left
                        anchors.rightMargin: 10
                        anchors.verticalCenter: searchBox.verticalCenter
                        implicitHeight: 30
                        leftPadding: 12
                        rightPadding: 14
                        text: I18n.t("Add computer")
                        Accessible.name: I18n.t("Add computer")
                        contentItem: Row {
                            spacing: 6
                            Icon { name: "plus"; size: 14; color: Theme.textOnAccent; anchors.verticalCenter: parent.verticalCenter }
                            Text {
                                text: addComputerButton.text
                                color: Theme.textOnAccent
                                font.pixelSize: 13
                                font.weight: Font.DemiBold
                                anchors.verticalCenter: parent.verticalCenter
                            }
                        }
                        onClicked: { home.refreshOwnComputers(); addDialog.openAdd(); }
                    }

                    Rectangle {
                        id: searchBox
                        visible: home.peerTab !== "server"
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.verticalCenterOffset: -3
                        width: 200
                        height: 30
                        radius: Theme.radiusSm
                        color: Theme.surface
                        border.color: searchInput.activeFocus ? Theme.accent : Theme.border
                        Icon { id: searchIcon; name: "search"; size: 14; color: Theme.textSubtle; anchors.left: parent.left; anchors.leftMargin: 9; anchors.verticalCenter: parent.verticalCenter }
                        TextInput {
                            id: searchInput
                            anchors.left: searchIcon.right
                            anchors.leftMargin: 7
                            anchors.right: parent.right
                            anchors.rightMargin: 8
                            anchors.verticalCenter: parent.verticalCenter
                            color: Theme.text
                            font.pixelSize: 12
                            clip: true
                            onTextChanged: home.searchText = text
                            Accessible.name: "Search computers"
                            Text { visible: searchInput.text.length === 0; text: I18n.t("Search"); color: Theme.textSubtle; font.pixelSize: 12; anchors.verticalCenter: parent.verticalCenter }
                        }
                    }
                }

                // Saved computers
                Item {
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: peerBar.bottom
                    anchors.bottom: parent.bottom
                    visible: home.peerTab !== "server"

                    GridView {
                        id: peerGrid
                        objectName: "peerGrid"
                        anchors.fill: parent
                        anchors.leftMargin: 24
                        anchors.topMargin: 16
                        clip: true
                        cellWidth: 232
                        cellHeight: 168
                        model: home.visiblePeers()
                        boundsBehavior: Flickable.StopAtBounds
                        delegate: Item {
                            width: peerGrid.cellWidth
                            height: peerGrid.cellHeight
                            PeerCard {
                                anchors.fill: parent
                                anchors.rightMargin: 16
                                anchors.bottomMargin: 16
                                cardData: modelData
                            }
                        }
                    }

                    Column {
                        anchors.centerIn: parent
                        visible: peerGrid.count === 0
                        spacing: 10
                        Icon {
                            anchors.horizontalCenter: parent.horizontalCenter
                            name: home.peerTab === "favorites" ? "star" : "clock"
                            size: 30
                            color: Theme.textSubtle
                        }
                        Text {
                            anchors.horizontalCenter: parent.horizontalCenter
                            text: home.searchText !== "" ? I18n.t("No computer matches \"%1\"").arg(home.searchText)
                                : (home.peerTab === "favorites" ? I18n.t("No favorites yet") : I18n.t("No recent connections"))
                            color: Theme.textMuted
                            font.pixelSize: 14
                            font.weight: Font.Medium
                        }
                        Text {
                            anchors.horizontalCenter: parent.horizontalCenter
                            visible: home.searchText === ""
                            text: home.peerTab === "favorites" ? I18n.t("Add a computer to favorites from its ⋮ menu.")
                                                              : I18n.t("Computers you add or connect to will show up here.")
                            color: Theme.textSubtle
                            font.pixelSize: 12
                        }
                    }
                }

                // Server tab, not signed in: run a server here (left) or sign
                // in to one (right).
                Flickable {
                    id: localServerPanel
                    visible: home.peerTab === "server" && !serverDashboard.isLoggedIn
                    anchors.left: parent.left
                    anchors.top: peerBar.bottom
                    anchors.bottom: parent.bottom
                    anchors.leftMargin: 24
                    anchors.topMargin: 16
                    width: Math.min(380, parent.width * 0.42)
                    contentHeight: localServerCard.implicitHeight + 24
                    clip: true
                    boundsBehavior: Flickable.StopAtBounds
                    LocalServerCard {
                        id: localServerCard
                        width: parent.width
                        homeView: home
                    }
                }

                // Lunaris server: sign-in and its computers
                Dashboard {
                    id: serverDashboard
                    embedded: true
                    anchors.left: localServerPanel.visible ? localServerPanel.right : parent.left
                    anchors.right: parent.right
                    anchors.top: peerBar.bottom
                    anchors.bottom: parent.bottom
                    anchors.fill: undefined
                    visible: home.peerTab === "server"
                    onCardMethodChosen: (card, method) => home.setPref("serverCardMethods", serverDashboard.cardMethods)
                    onStartSessionRequested: (server, token, hostId, hostName, appId, res, fps, codec, bitrate, queueLimit, disableCuda, renderBackend, inputProtocol, encoder, displayId, sourceId) => {
                        // Not to a saved computer: what the session lists belongs to the Server tab.
                        home.sessionPeer = "";
                        home.sessionSource = "";
                        home.setStatus(I18n.t("Connecting to %1…").arg(hostName), false);
                        home.startSessionRequested(server, token, hostId, hostName, appId, res, fps, codec, bitrate, queueLimit, disableCuda, renderBackend, inputProtocol, encoder, displayId, sourceId);
                    }
                }
            }
        }

        // ===========================================================
        // SETTINGS
        // ===========================================================
        AppSettings {
            anchors.fill: parent
            visible: home.page === "settings"
            homeView: home
        }
    }

    // ---------------------------------------------------------------
    // Saved computer card
    // ---------------------------------------------------------------
    component PeerCard: Rectangle {
        id: card
        objectName: "peerCard"
        // The computer (Cards.js: its ways to connect) and the way that is
        // chosen for it. Everything on the card acts on that way: `peer` is
        // its entry in the list of saved computers.
        property var cardData: ({ key: "", title: "", methods: [], peers: [] })
        readonly property bool multi: cardData.methods.length > 1
        readonly property var method: home.methodOf(cardData)
        readonly property var peer: method ? home.methodPeer(method) : ({})
        // The entry the card's star is kept with.
        readonly property var firstPeer: cardData.peers.length > 0 ? cardData.peers[0] : peer
        // A computer of another type: a badge says which, there is no
        // address of its own to check or to show, and no Lunaris files.
        readonly property bool isSource: home.isSource(peer)
        readonly property string title: multi ? cardData.title : (peer.name ? peer.name : home.displayAddress(peer.address || ""))
        readonly property var cardColors: ["#3b4a6b", "#4a3b6b", "#2f5560", "#5a4632", "#3e5a3a", "#5c3a4a"]
        radius: Theme.radius
        color: Theme.surface
        border.color: cardMa.containsMouse ? Theme.borderStrong : Theme.border
        clip: true

        function colorFor(text) {
            var h = 0;
            for (var i = 0; i < text.length; i++) h = (h * 31 + text.charCodeAt(i)) % 997;
            return cardColors[h % cardColors.length];
        }

        MouseArea {
            id: cardMa
            objectName: "peerCardArea"
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onDoubleClicked: home.connectMethod(card.cardData, card.method)
            onClicked: home.connectMethod(card.cardData, card.method)
        }

        Rectangle {
            id: cardTop
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            height: parent.height - 44
            radius: Theme.radius
            color: card.colorFor(card.firstPeer.address || "")
            // square off the bottom corners
            Rectangle { anchors.left: parent.left; anchors.right: parent.right; anchors.bottom: parent.bottom; height: Theme.radius; color: parent.color }

            Column {
                anchors.centerIn: parent
                // (Below the ways to connect, when there are several.)
                anchors.verticalCenterOffset: card.multi ? (methodChips.height + 9) / 2 : 0
                spacing: card.multi ? 4 : 8
                Icon { anchors.horizontalCenter: parent.horizontalCenter; name: "monitor"; size: card.multi ? 24 : 34; color: "#ffffff"; strokeWidth: 1.6 }
                Text {
                    objectName: "peerCardName"
                    anchors.horizontalCenter: parent.horizontalCenter
                    width: card.width - 24
                    horizontalAlignment: Text.AlignHCenter
                    textFormat: Text.PlainText
                    text: card.multi ? card.cardData.title : (card.peer.name ? card.peer.name : (card.peer.os || I18n.t("Computer")))
                    color: Qt.rgba(1, 1, 1, 0.85)
                    font.pixelSize: 12
                    elide: Text.ElideRight
                }
            }
            Icon {
                id: favoriteStar
                visible: !!card.firstPeer.favorite
                anchors.right: parent.right
                anchors.top: parent.top
                anchors.margins: 10
                name: "star"
                size: 15
                color: "#fcd34d"
            }
            TypeBadge {
                visible: card.isSource && !card.multi
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 9
                onColor: true
                typeName: card.isSource ? home.typeName(card.peer) : ""
                maxWidth: card.width - 18 - (favoriteStar.visible ? 26 : 0)
            }
            // The ways to connect, when there is more than one: a click
            // chooses the one the card acts on (two rows at most; all of
            // them are in the card's menu).
            MethodChips {
                id: methodChips
                objectName: "peerChips"
                dots: false
                visible: card.multi
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 9
                width: card.width - 18 - (favoriteStar.visible ? 26 : 0)
                height: card.multi ? Math.min(implicitHeight, 45) : 0
                clip: true
                z: 2
                onColor: true
                methods: card.multi ? card.cardData.methods : []
                current: card.method ? card.method.key : ""
                onChosen: (key) => home.chooseMethod(card.cardData, key)
            }
        }

        RowLayout {
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            height: 44
            anchors.leftMargin: 12
            anchors.rightMargin: 6
            spacing: 6
            Rectangle {
                readonly property int online: card.isSource ? -1 : home.peerOnline(card.peer.address || "")
                visible: online >= 0
                width: 8; height: 8; radius: 4
                color: online === 1 ? "#22c55e" : Theme.textMuted
                ToolTip.visible: dotMa.containsMouse
                ToolTip.text: online === 1 ? I18n.t("Online") : I18n.t("Offline")
                MouseArea { id: dotMa; anchors.fill: parent; hoverEnabled: true }
            }
            Text {
                Layout.fillWidth: true
                textFormat: Text.PlainText
                text: card.isSource ? home.sourceSummary(card.peer) : home.displayAddress(card.peer.address || "")
                color: Theme.textStrong
                font.pixelSize: 13
                font.weight: Font.DemiBold
                elide: Text.ElideRight
            }
            IconButton {
                objectName: "peerFilesButton"
                visible: !card.isSource || home.sourceHasFiles(card.peer)
                iconName: "folder"
                tip: I18n.t("Files")
                onClicked: home.openPeerFiles(card.peer.address)
            }
            IconButton {
                iconName: "settings"
                tip: I18n.t("Stream settings")
                onClicked: home.openPeerSettings(card.peer.address)
            }
            IconButton {
                id: moreBtn
                objectName: "peerMoreButton"
                iconName: "more"
                tip: I18n.t("More")
                onClicked: peerMenu.popup(moreBtn, 0, moreBtn.height)
            }
        }

        ThemedMenu {
            id: peerMenu
            objectName: "peerMenu"
            width: card.multi ? 250 : implicitWidth
            MenuItem {
                visible: !card.multi
                height: visible ? implicitHeight : 0
                text: I18n.t("Connect")
                onTriggered: home.connectMethod(card.cardData, card.method)
            }
            // Several ways to connect: each, to connect with it at once.
            // (Put in by hand, after "Connect", which a card with several has not.)
            Instantiator {
                model: card.multi ? card.cardData.methods : []
                onObjectAdded: (index, object) => peerMenu.insertItem(index + 1, object)
                onObjectRemoved: (index, object) => peerMenu.removeItem(object)
                delegate: MenuItem {
                    id: methodEntry
                    required property var modelData
                    objectName: "peerMethodItem"
                    property string methodKey: modelData.key
                    text: I18n.t("Connect with %1").arg(modelData.label)
                    leftPadding: 36
                    TypeLogo {
                        x: 10
                        anchors.verticalCenter: parent.verticalCenter
                        typeId: Cards.methodType(methodEntry.modelData)
                        name: String(methodEntry.modelData.label)
                        size: 18
                    }
                    contentItem: Text {
                        textFormat: Text.PlainText
                        text: methodEntry.text
                        color: Theme.text
                        font.pixelSize: 13
                        elide: Text.ElideRight
                        verticalAlignment: Text.AlignVCenter
                    }
                    onTriggered: home.connectMethod(card.cardData, modelData)
                }
            }
            MenuItem {
                text: card.firstPeer.favorite ? I18n.t("Remove from favorites") : I18n.t("Add to favorites")
                onTriggered: home.updatePeer(card.firstPeer.address, { favorite: !card.firstPeer.favorite })
            }
            // What a card of another type has not is left out, not greyed.
            MenuItem {
                objectName: "peerFilesItem"
                visible: !card.isSource || home.sourceHasFiles(card.peer)
                height: visible ? implicitHeight : 0
                text: I18n.t("Files")
                onTriggered: home.openPeerFiles(card.peer.address)
            }
            MenuItem { text: I18n.t("Stream settings"); onTriggered: home.openPeerSettings(card.peer.address) }
            MenuItem {
                visible: !card.isSource
                height: visible ? implicitHeight : 0
                text: I18n.t("Rename…")
                onTriggered: renameDialog.openFor(card.peer)
            }
            MenuItem {
                visible: card.isSource
                height: visible ? implicitHeight : 0
                text: I18n.t("Edit…")
                onTriggered: home.editSource(card.peer)
            }
            MenuItem {
                visible: !card.isSource
                height: visible ? implicitHeight : 0
                text: I18n.t("Forget password")
                enabled: !!card.peer.password
                onTriggered: home.updatePeer(card.peer.address, { password: "" })
            }
            MenuItem {
                objectName: "peerAddMethodItem"
                text: I18n.t("Add a connection method…")
                onTriggered: home.addMethodTo(card.cardData)
            }
            MenuSeparator {}
            MenuItem {
                objectName: "peerRemoveItem"
                text: I18n.t("Remove")
                onTriggered: card.isSource ? home.removeSourcePeer(card.peer) : home.removePeer(card.peer.address)
            }
        }
    }

    // ---------------------------------------------------------------
    // Dialogs
    // ---------------------------------------------------------------
    // Clipboard via a hidden TextEdit (no extra module needed).
    QtObject {
        id: clipboardHelper
        function copy(text) {
            clipboardEdit.text = text;
            clipboardEdit.selectAll();
            clipboardEdit.copy();
            clipboardEdit.text = "";
        }
    }
    TextEdit { id: clipboardEdit; visible: false }

    DialogFrame {
        id: passwordDialog
        property string address: ""
        // What to do with the password (connect when unset).
        property var action: null
        function openFor(addr, then) {
            address = addr;
            action = then || null;
            pwField.text = "";
            rememberBox.checked = true;
            open();
            pwField.input.forceActiveFocus();
        }
        function accept() {
            if (pwField.text.trim() === "") return;
            close();
            var pw = pwField.text.trim();
            if (action) {
                if (rememberBox.checked) home.rememberPeer(address, pw, true);
                action(pw);
            } else {
                home.connectTo(address, pw, rememberBox.checked);
            }
        }
        // No password: the person at that computer is asked to accept.
        function askThem() {
            close();
            if (action) action("");
            else home.connectTo(address, "", false);
        }
        contentItem: ColumnLayout {
            spacing: 12
            Text { textFormat: Text.PlainText; text: I18n.t("Password for %1").arg(home.displayAddress(passwordDialog.address)); color: Theme.textStrong; font.pixelSize: 16; font.weight: Font.DemiBold }
            Text {
                Layout.fillWidth: true
                text: I18n.t("Enter the password shown under \"This computer\" on that computer.")
                color: Theme.textMuted
                font.pixelSize: 12
                wrapMode: Text.WordWrap
            }
            InputField { id: pwField; placeholder: I18n.t("Password"); password: true; onAccepted: passwordDialog.accept() }
            CheckRow { id: rememberBox; text: I18n.t("Remember password"); checked: true }
            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                SecondaryButton {
                    text: I18n.t("Ask to connect")
                    ToolTip.visible: hovered
                    ToolTip.delay: 400
                    ToolTip.text: I18n.t("Without the password: the person at that computer is asked to accept.")
                    onClicked: passwordDialog.askThem()
                }
                Item { Layout.fillWidth: true }
                SecondaryButton { text: I18n.t("Cancel"); onClicked: passwordDialog.close() }
                PrimaryButton { text: I18n.t("Connect"); onClicked: passwordDialog.accept() }
            }
        }
    }

    DialogFrame {
        id: renameDialog
        property string address: ""
        function openFor(peer) {
            address = peer.address;
            nameField.text = peer.name || "";
            open();
            nameField.input.forceActiveFocus();
        }
        function accept() {
            home.updatePeer(address, { name: nameField.text.trim() });
            close();
        }
        contentItem: ColumnLayout {
            spacing: 12
            Text { textFormat: Text.PlainText; text: I18n.t("Rename %1").arg(home.displayAddress(renameDialog.address)); color: Theme.textStrong; font.pixelSize: 16; font.weight: Font.DemiBold }
            InputField { id: nameField; placeholder: I18n.t("Name, e.g. Office PC"); onAccepted: renameDialog.accept() }
            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Item { Layout.fillWidth: true }
                SecondaryButton { text: I18n.t("Cancel"); onClicked: renameDialog.close() }
                PrimaryButton { text: I18n.t("Save"); onClicked: renameDialog.accept() }
            }
        }
    }

    // "Add computer" (button above the list) and "Edit…" of a computer of
    // another type.
    AddComputerDialog {
        id: addDialog
        objectName: "addComputerDialog"
        connectors: home.connectors
        onLunarisSubmitted: (address, password, name) => {
            var error = home.addLunarisPeer(address, password, name);
            if (error !== "") { addDialog.error = error; return; }
            addDialog.close();
        }
        onSourceSubmitted: (connectorId, hostId, sourceId, name, values, group) => {
            var error = home.saveSource(connectorId, sourceId, name, values, group);
            if (error !== "") { addDialog.error = error; return; }
            // A set-up (pairing) keeps the dialog open until it is done.
            if (addDialog.setupShown) return;
            addDialog.close();
        }
        onSetupAsked: (setup, hostId) => home.askSourceSetup(setup)
        onSetupCancelled: (setup, hostId) => {
            bridge.bridgeCancelSetup(setup);
            // It may have been done in that very moment: then the computer
            // is there and gets its card.
            home.refreshOwnComputers();
            home.adoptOwnComputers();
        }
    }

    DialogFrame {
        id: customPasswordDialog
        property string error: ""
        onOpened: { customField.text = ""; error = ""; customField.input.forceActiveFocus(); }
        function accept() {
            var err = home.setHostPassword(customField.text);
            if (err !== "") { error = err; return; }
            home.passwordVisible = true;
            home.setStatus(I18n.t("Password changed. Computers using the old one can no longer connect."), false);
            close();
        }
        contentItem: ColumnLayout {
            spacing: 12
            Text { text: I18n.t("Set the password"); color: Theme.textStrong; font.pixelSize: 16; font.weight: Font.DemiBold }
            Text {
                Layout.fillWidth: true
                text: I18n.t("Anyone with this computer's address and this password can control it. Use at least 6 characters.")
                color: Theme.textMuted
                font.pixelSize: 12
                wrapMode: Text.WordWrap
            }
            InputField { id: customField; placeholder: I18n.t("New password"); onAccepted: customPasswordDialog.accept() }
            Text { visible: customPasswordDialog.error !== ""; text: customPasswordDialog.error; color: Theme.danger; font.pixelSize: 12 }
            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Item { Layout.fillWidth: true }
                SecondaryButton { text: I18n.t("Cancel"); onClicked: customPasswordDialog.close() }
                PrimaryButton { text: I18n.t("Save"); onClicked: customPasswordDialog.accept() }
            }
        }
    }

    // Stream settings of a saved computer (gear on its card), kept with it.
    Settings {
        id: peerSettings
        objectName: "peerSettings"
        property string address: ""
        property string password: ""
        anchors.fill: parent
        z: 1000
        onApplySettings: (res, fps, codec, bitrate, queueLimit, disableCuda, renderBackend, inputProtocol, encoder, displayId, sourceId) => {
            home.updatePeer(peerSettings.address, { settings: {
                res: res, fps: fps, codec: codec, bitrate: bitrate, queueLimit: queueLimit,
                renderBackend: renderBackend, inputProtocol: inputProtocol, encoder: encoder,
                displayId: displayId, sourceId: sourceId
            } });
        }
    }
}
