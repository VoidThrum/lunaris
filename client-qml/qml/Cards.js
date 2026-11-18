.pragma library

// One card for each computer.
//
// A computer can be listed in more than one way: as a Lunaris computer (a
// host), and as sources (the same machine reached with RustDesk, with
// Sunshine, ...). A `group` (an opaque word) says which are one computer:
// sources with the same group are one computer, and the group `host:<id>`
// is the Lunaris computer with that id. This file turns the two lists into
// cards, each with its ways to connect ("methods").
//
// Nothing here widens what a person may use: the lists are the ones this
// person was given, and a source joins a Lunaris computer's card only when
// that computer is in the list too.
//
// The web client makes its cards the same way (web/src/computerCards.ts is
// this file's twin: the same rules and the same keys); change neither alone.

var HOST_GROUP = "host:";
var MAX_GROUP = 64;

// Whether `group` can be a group (as the server and the agents check it).
function validGroup(group) {
    return typeof group === "string" && group.length > 0 && group.length <= MAX_GROUP && /^[A-Za-z0-9._:-]+$/.test(group);
}

// A new group for sources that are to be one computer.
function newGroup() {
    var hex = "";
    for (var i = 0; i < 12; i++) hex += Math.floor(Math.random() * 16).toString(16);
    return "g-" + hex;
}

// "RustDesk (by IP address)" reads "RustDesk" on a chip.
function shortTypeName(name) {
    var text = String(name || "");
    return text.replace(/\s*[(（].*$/, "").trim() || text;
}

// The types whose short name is not their name without its brackets.
var SHORT_NAMES = { lunaris: "Lunaris", rustdesk: "RustDesk", sunshine: "Sunshine" };

// The name of a type where there is little room (a tile, a chip): by its
// connector's id for the types that have a name of their own, else the
// connector's name without its brackets. The full name stays for a tool tip.
function typeShortName(connector, name) {
    var id = String(connector || "");
    if (Object.prototype.hasOwnProperty.call(SHORT_NAMES, id)) return SHORT_NAMES[id];
    return shortTypeName(name || id);
}

// The types that have a logo of their own (TypeLogo.qml), by connector id.
var LOGOS = { lunaris: "icon.png", rustdesk: "logos/rustdesk.svg", sunshine: "logos/sunshine.svg" };
function typeLogo(connector) {
    var id = String(connector || "");
    return Object.prototype.hasOwnProperty.call(LOGOS, id) ? LOGOS[id] : "";
}
// The type of a way to connect ({ host | source }): "lunaris" or its connector's id.
function methodType(method) {
    return method && method.source ? String(method.source.connector || "") : "lunaris";
}

// ---- The form of a type: which fields share a row ----

// A field that needs little room: a port, or one whose usual value is short.
function shortField(field) {
    var usual = String(field["default"] || "");
    return field.kind === "port" || (field.kind === "text" && usual !== "" && usual.length <= 12);
}

// The fields of a form on a wide window, in rows of 6 columns: a field and
// the port after it share a row (4 + 2); short fields that follow one
// another share rows, three to a row (2 + 2 + 2) and two (3 + 3) where three
// would leave one alone; anything else has a row of its own.
// [{ field, span }], in the fields' order. (On a narrow window every field
// has its own row.)
function layoutFields(fields) {
    var placed = [];
    fields = fields || [];
    // A field that takes the port after it onto its row.
    function withPort(i) {
        return !!fields[i] && fields[i].kind !== "port" && fields[i].kind !== "choice"
            && !!fields[i + 1] && fields[i + 1].kind === "port";
    }
    var i = 0;
    while (i < fields.length) {
        if (withPort(i)) {
            placed.push({ field: fields[i], span: 4 }, { field: fields[i + 1], span: 2 });
            i += 2;
            continue;
        }
        var end = i;
        while (end < fields.length && shortField(fields[end]) && !withPort(end)) end++;
        var run = end - i;
        if (run < 2) {
            placed.push({ field: fields[i], span: 6 });
            i++;
            continue;
        }
        // Rows of three; a rest of one takes one from the row before it (two rows of two).
        var left = run;
        while (left > 0) {
            var row = left === 4 || left === 2 ? 2 : 3;
            for (var k = 0; k < row; k++) placed.push({ field: fields[i++], span: row === 3 ? 2 : 3 });
            left -= row;
        }
    }
    return placed;
}

function sourceKey(source) {
    return "source:" + source.host_id + ":" + source.id;
}

function byName(a, b) {
    return String(a.name).localeCompare(String(b.name)) || String(a.id).localeCompare(String(b.id))
        || String(a.host_id).localeCompare(String(b.host_id));
}

function sourceMethod(source, label) {
    return { key: sourceKey(source), source: source, label: label(source), online: !!source.online };
}

// The cards: the Lunaris computers in the order given, each with the
// sources that are that computer; then the other sources by name, those of
// one group on one card (named after the first of them).
//
// `label(source)` names a source's method (its type); `hostGroup(host)` is
// the group a Lunaris computer is (left out: "host:<its id>").
// A card: { key, title, host (or undefined), group, methods: [{ key, label,
// online, host | source }] }.
function buildCards(hosts, sources, label, lunarisLabel, hostGroup) {
    label = label || function (source) { return typeShortName(source.connector, source.connector_name); };
    lunarisLabel = lunarisLabel || "Lunaris";
    hostGroup = hostGroup || function (host) { return HOST_GROUP + host.id; };
    var sorted = (sources || []).slice().sort(byName);
    function groupOf(source) { return validGroup(source.group) ? source.group : ""; }
    var cards = [];
    var taken = [];
    var i, k;

    for (i = 0; i < (hosts || []).length; i++) {
        var host = hosts[i];
        var group = hostGroup(host);
        // A computer whose id cannot be part of a group takes no methods.
        if (!validGroup(group)) group = "";
        var methods = [{ key: "lunaris", host: host, label: lunarisLabel, online: host.status === "Online" }];
        for (k = 0; k < sorted.length; k++) {
            if (group !== "" && taken.indexOf(sorted[k]) === -1 && groupOf(sorted[k]) === group) {
                taken.push(sorted[k]);
                methods.push(sourceMethod(sorted[k], label));
            }
        }
        cards.push({ key: "host:" + host.id, title: host.name, host: host, group: group, methods: methods });
    }

    var byGroup = {};
    for (k = 0; k < sorted.length; k++) {
        var source = sorted[k];
        if (taken.indexOf(source) !== -1) continue;
        var g = groupOf(source);
        if (g !== "" && byGroup[g]) {
            byGroup[g].methods.push(sourceMethod(source, label));
            continue;
        }
        var card = { key: g !== "" ? "group:" + g : sourceKey(source), title: source.name, host: undefined, group: g, methods: [sourceMethod(source, label)] };
        if (g !== "") byGroup[g] = card;
        cards.push(card);
    }

    // Two methods of one type on a card are told apart by their names.
    for (i = 0; i < cards.length; i++) {
        var count = {};
        var ms = cards[i].methods;
        for (k = 0; k < ms.length; k++) if (ms[k].source) count[ms[k].label] = (count[ms[k].label] || 0) + 1;
        for (k = 0; k < ms.length; k++) if (ms[k].source && count[ms[k].label] > 1) ms[k].label = ms[k].label + ": " + ms[k].source.name;
    }
    return cards;
}

// The method a card connects with: the one remembered for it when it is
// still there and can be connected to; else the Lunaris computer itself, or
// the first method, when that can be; else the first that can be; else (the
// whole card is offline) the remembered one or the first.
function currentMethod(methods, remembered) {
    if (!methods || methods.length === 0) return undefined;
    var kept;
    var i;
    if (remembered) for (i = 0; i < methods.length; i++) if (methods[i].key === remembered) { kept = methods[i]; break; }
    if (kept && kept.online) return kept;
    var first = methods[0];
    if (!kept && first.online) return first;
    for (i = 0; i < methods.length; i++) if (methods[i].online) return methods[i];
    return kept || first;
}

// The most cards a method is remembered for (the oldest are forgotten).
var MAX_REMEMBERED = 200;

// `known` ({ "<card key>": "<method key>" }) with `method` remembered for `card`.
function remembered(known, card, method) {
    var next = {};
    var keys = Object.keys(known || {}).filter(function (key) { return key !== card && typeof known[key] === "string"; });
    keys = keys.slice(Math.max(0, keys.length - (MAX_REMEMBERED - 1)));
    for (var i = 0; i < keys.length; i++) next[keys[i]] = known[keys[i]];
    next[card] = method;
    return next;
}

// The card of `cards` that has the method `key`, or undefined.
function cardWithMethod(cards, key) {
    for (var i = 0; i < cards.length; i++)
        for (var k = 0; k < cards[i].methods.length; k++)
            if (cards[i].methods[k].key === key) return cards[i];
    return undefined;
}
