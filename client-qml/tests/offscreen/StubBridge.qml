import QtQuick

// Stands in for the Rust `bridge` (StreamBridge) when the pages are
// rendered off screen: the functions and signals Home.qml, Dashboard.qml
// and the dialogs use, answering from canned data. Nothing leaves this
// process and nothing is stored.
QtObject {
    id: stub

    // ---- What the tests set ----
    // "admin" | "user"; the canned server's answers.
    property string role: "admin"
    // A server from before sources: 404 to /api/sources and /api/connectors.
    property bool oldServer: false
    // The answer to the next write of a source: { status, body }, or null (200).
    property var nextWrite: null
    // What the pages asked for, newest last: { method, path, body }.
    property var calls: []
    // The session the pages started last (startSessionRequested is theirs;
    // this is for fetchCapabilities and friends).
    property var probed: []
    property var capabilitiesAsked: []
    property var stores: ({})

    readonly property var rustdesk: ({
        id: "rustdesk", name: "RustDesk (by IP address)",
        fields: [
            { key: "host", label: "IP address", kind: "text", "default": "", required: true },
            { key: "port", label: "Port", kind: "port", "default": "21118", required: false },
            { key: "password", label: "Password", kind: "secret", "default": "", required: true }
        ]
    })
    // A made-up connector with other fields, to show the form is built from data.
    readonly property var acme: ({
        id: "acme-view", name: "Acme Viewer",
        fields: [
            { key: "station", label: "Station name", kind: "text", "default": "", required: true },
            { key: "gateway", label: "Gateway", kind: "text", "default": "gw.example.net", required: false },
            { key: "gateway_port", label: "Gateway port", kind: "port", "default": "7400", required: true },
            { key: "token", label: "Access token", kind: "secret", "default": "", required: true },
            { key: "pin", label: "PIN", kind: "secret", "default": "", required: false }
        ]
    })
    readonly property var firstOwnSources: [
        { id: "den-pc-1a2b", name: "Den PC", connector: "rustdesk", connector_name: "RustDesk (by IP address)", values: { host: "192.168.1.50", port: "21118" } }
    ]
    property var ownSources: firstOwnSources
    property var hosts: [
        { id: "0F3A9C2E-77B1-4C1D-9A55-1B2C3D4E5F60", name: "Office PC", status: "Online", address: "192.168.1.20:8091" },
        { id: "5D21B7AA-0C4E-4F7B-8E11-99A0B1C2D3E4", name: "Lab PC", status: "Offline", ip_address: "10.0.0.7" }
    ]
    readonly property var firstServerSources: [
        { host_id: "0F3A9C2E-77B1-4C1D-9A55-1B2C3D4E5F60", host_name: "Office PC", id: "lab-server-9f31", name: "Lab server", connector: "rustdesk", connector_name: "RustDesk (by IP address)", online: true },
        { host_id: "5D21B7AA-0C4E-4F7B-8E11-99A0B1C2D3E4", host_name: "Lab PC", id: "press-7c02", name: "Press room", connector: "acme-view", connector_name: "Acme Viewer", online: false }
    ]
    property var serverSources: firstServerSources

    // Back to the data above, as a fresh start has it.
    function startOver() {
        role = "admin";
        oldServer = false;
        nextWrite = null;
        calls = [];
        probed = [];
        capabilitiesAsked = [];
        ownSources = firstOwnSources;
        serverSources = firstServerSources;
    }

    // ---- Signals of the real bridge that the pages listen to ----
    signal loginResult(bool success, string errorMsg, string tok, string user, string srv)
    signal credentialsLoaded(bool success, string srv, string tok, string user)
    signal hostsResult(bool success, string errorMsg, string hostsJson)
    signal appsResult(bool success, string errorMsg, string hostId, string appsJson)
    signal agentTokenResult(bool success, string errorMsg, string tok)
    signal apiResult(string reqId, int status, string body)
    signal capabilitiesUpdated(string displaysJson, string encodersJson, string gpuInfo, string hostOs, string sourcesJson)

    // ---- Stores and status ----
    function loadStore(name) { return stub.stores[name] || ""; }
    function saveStore(name, json) { var next = Object.assign({}, stub.stores); next[name] = json; stub.stores = next; }
    function appVersion() { return "0.5.0"; }
    function screenRefreshRate() { return 60; }
    function savedDirectAddress() { return ""; }
    function savedDirectPassword() { return ""; }
    function saveDirectTarget(address, password) {}
    function getLocalHostname() { return "this-pc"; }
    function isAutostartEnabled() { return false; }
    function setAutostartEnabled(on) {}
    function checkForUpdates() {}
    function hostStatus() {
        return JSON.stringify({ enabled: true, running: true, port: 8091, password: "k7m2p9qd", addresses: ["192.168.1.10"],
            hostname: "this-pc", input_ready: true, allow_clipboard: true, allow_file_transfer: true, full_disk_access: false,
            approve_mode: "password-click", rustdesk_enabled: false, rustdesk_port: 21118, error: "" });
    }
    function hostStart() { return ""; }
    function hostStop() { return ""; }
    function hostSetPassword(p) { return ""; }
    function hostSetPort(p) { return ""; }
    function hostSetRustdesk(on, port) { return ""; }
    function hostSetSharing(c, f) { return ""; }
    function hostSetFileAccess(full) { return ""; }
    function hostSetApproveMode(mode) { return ""; }
    function hostUseServer(server, token) { return "already"; }
    function serverStatus() {
        return JSON.stringify({ enabled: false, running: false, ours: false, port: 8090, local_url: "", lan_url: "", admin_password: "", data_dir: "", error: "" });
    }
    function serverStart() { return ""; }
    function serverStop() { return ""; }
    function serverSetPort(p) { return ""; }
    function probePeer(address) { stub.probed = stub.probed.concat([address]); }
    function peerOnline(address) { return address.indexOf("192.168.1.20") === 0 ? 1 : 0; }
    function fetchCapabilities(server, token, hostId) { stub.capabilitiesAsked = stub.capabilitiesAsked.concat([server + " " + hostId]); }
    function fetchAgentToken(server, token) {}
    function deleteHost(id) {}
    function logout() {}
    function login(server, user, password) {}

    // ---- The app's own computers of other types ----
    function bridgeConnectors() { return JSON.stringify([stub.rustdesk, stub.acme]); }
    function bridgeSources() { return JSON.stringify(stub.ownSources); }
    function shownValues(connectorId, values) {
        var c = connectorId === "rustdesk" ? stub.rustdesk : stub.acme;
        var out = {};
        for (var i = 0; i < c.fields.length; i++)
            if (c.fields[i].kind !== "secret" && values[c.fields[i].key]) out[c.fields[i].key] = values[c.fields[i].key];
        return out;
    }
    function bridgeAddSource(name, connector, valuesJson) {
        var c = connector === "rustdesk" ? stub.rustdesk : stub.acme;
        var id = name.toLowerCase().replace(/[^a-z0-9]+/g, "-") + "-0001";
        stub.ownSources = stub.ownSources.concat([{ id: id, name: name, connector: connector, connector_name: c.name, values: shownValues(connector, JSON.parse(valuesJson)) }]);
        return JSON.stringify({ id: id });
    }
    function bridgeUpdateSource(id, name, valuesJson) {
        var found = false;
        stub.ownSources = stub.ownSources.map(function (s) {
            if (s.id !== id) return s;
            found = true;
            return Object.assign({}, s, { name: name, values: Object.assign({}, s.values, shownValues(s.connector, JSON.parse(valuesJson))) });
        });
        return JSON.stringify(found ? { id: id } : { error: "That computer is not there any more" });
    }
    function bridgeRemoveSource(id) {
        stub.ownSources = stub.ownSources.filter(function (s) { return s.id !== id; });
        return "";
    }

    // ---- The canned server ----
    function loadSavedCredentials() { stub.credentialsLoaded(true, "http://192.168.1.5:8090", "session-token", "huy"); }
    function fetchHosts() { stub.hostsResult(true, "", JSON.stringify(stub.hosts)); }
    function apiRequest(reqId, method, path, body) {
        stub.calls = stub.calls.concat([{ method: method, path: path, body: body }]);
        var status = 200;
        var answer = "{}";
        if (path === "/api/auth/me") {
            answer = JSON.stringify({ username: "huy", role: stub.role });
        } else if (path === "/api/sources" || path === "/api/connectors") {
            if (stub.oldServer) { status = 404; answer = ""; }
            else if (path === "/api/sources") answer = JSON.stringify(stub.serverSources);
            else answer = JSON.stringify([{ host_id: stub.hosts[0].id, host_name: stub.hosts[0].name, connectors: [stub.rustdesk, stub.acme] }]);
        } else if (/^\/api\/hosts\/[^/]+\/sources/.test(path)) {
            if (stub.nextWrite) { status = stub.nextWrite.status; answer = stub.nextWrite.body; stub.nextWrite = null; }
            else if (method === "POST") {
                var form = JSON.parse(body);
                stub.serverSources = stub.serverSources.concat([{ host_id: stub.hosts[0].id, host_name: stub.hosts[0].name, id: "new-0001", name: form.name, connector: form.connector, connector_name: form.connector === "rustdesk" ? stub.rustdesk.name : stub.acme.name, online: true }]);
                answer = JSON.stringify({ id: "new-0001" });
            } else if (method === "DELETE") {
                var id = path.slice(path.lastIndexOf("/") + 1);
                stub.serverSources = stub.serverSources.filter(function (s) { return s.id !== id; });
            } else {
                answer = JSON.stringify({ id: path.slice(path.lastIndexOf("/") + 1) });
            }
        }
        stub.apiResult(reqId, status, answer);
    }
}
