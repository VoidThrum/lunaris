// One card for each computer.
//
// A computer can be listed in more than one way: as a Lunaris computer (a
// host), and as sources of Lunaris computers (the same machine reached with
// RustDesk, with Sunshine, ...). The server says which sources are one
// computer with a `group` (an opaque word): sources with the same group are
// one computer, and the group `host:<id>` is the Lunaris computer with that
// id. This file turns the two lists into cards, each with its ways to
// connect ("methods"), and remembers which way was used last for a card.
//
// Nothing here widens what a person may use: the lists are the ones the
// server gave this person, and a source joins a Lunaris computer's card only
// when that computer is in this person's list too.
//
// The desktop client makes its cards the same way (client-qml/qml/Cards.js);
// change neither alone.

/** What a card needs of a Lunaris computer. */
export interface CardHost {
  id: string;
  name: string;
  status: string;
}

/** What a card needs of a source. */
export interface CardSource {
  host_id: string;
  host_name: string;
  id: string;
  name: string;
  connector: string;
  connector_name: string;
  online: boolean;
  /** Which computer it is, when that computer is listed in other ways too. */
  group?: string;
}

/** A way to connect to a card's computer. */
export interface Method<H extends CardHost = CardHost, S extends CardSource = CardSource> {
  /** Stable within its card: "lunaris", or "source:<host id>:<source id>". */
  key: string;
  /** The Lunaris computer itself (then `source` is not set). */
  host?: H;
  /** A source: the computer reached with another protocol. */
  source?: S;
  /** For a chip: "Lunaris", or the connector's name without its brackets. */
  label: string;
  /** Whether it can be connected to now. */
  online: boolean;
}

export interface Card<H extends CardHost = CardHost, S extends CardSource = CardSource> {
  /** Stable for the computer: "host:<id>", "group:<group>" or "source:<host id>:<source id>". */
  key: string;
  title: string;
  /** The Lunaris computer this card is, when it is one. */
  host?: H;
  /** The group new methods of this card are added with ('' when it has none yet). */
  group: string;
  methods: Method<H, S>[];
}

export const HOST_GROUP = 'host:';
export const MAX_GROUP = 64;

/** Whether `group` can be a group (as the server and the agents check it). */
export const validGroup = (group: unknown): group is string =>
  typeof group === 'string' && group.length > 0 && group.length <= MAX_GROUP && /^[A-Za-z0-9._:-]+$/.test(group);

/** A new group for sources that are to be one computer. */
export const newGroup = (random: () => number = Math.random): string => {
  let hex = '';
  for (let i = 0; i < 12; i++) hex += Math.floor(random() * 16).toString(16);
  return `g-${hex}`;
};

/** "RustDesk (by IP address)" reads "RustDesk" on a chip. */
export const shortTypeName = (name: string): string => name.replace(/\s*[(（].*$/, '').trim() || name;

/** The types whose short name is not their name without its brackets. */
const SHORT_NAMES: Record<string, string> = { lunaris: 'Lunaris', rustdesk: 'RustDesk', sunshine: 'Sunshine' };

/**
 * The name of a type where there is little room (a tile, a chip): by its
 * connector's id for the types that have a name of their own, else the
 * connector's name without its brackets. The full name stays for a tool tip.
 */
export const typeShortName = (connector: string, name: string): string =>
  (Object.prototype.hasOwnProperty.call(SHORT_NAMES, connector) ? SHORT_NAMES[connector] : '') || shortTypeName(name || connector);

// ---- The form of a type: which fields share a row ----

/** What the layout needs of a field of a connector's form. */
export interface LayoutField {
  key: string;
  kind: string;
  default?: string;
}

/** A field in its row: `span` of 6 columns (6 = the whole row). */
export interface PlacedField<F extends LayoutField = LayoutField> {
  field: F;
  span: 2 | 3 | 4 | 6;
}

/** A field that needs little room: a port, or one whose usual value is short. */
const shortField = (field: LayoutField) =>
  field.kind === 'port' || (field.kind === 'text' && !!field.default && field.default.length <= 12);

/**
 * The fields of a form on a wide window, in rows of 6 columns: a field and
 * the port after it share a row (4 + 2); short fields that follow one
 * another share rows, three to a row (2 + 2 + 2) and two (3 + 3) where three
 * would leave one alone; anything else has a row of its own. On a narrow
 * window every field has its own row (the caller's style sheet). The order
 * never changes.
 */
export function layoutFields<F extends LayoutField>(fields: F[]): PlacedField<F>[] {
  const placed: PlacedField<F>[] = [];
  // A field that takes the port after it onto its row.
  const withPort = (i: number) =>
    !!fields[i] && fields[i].kind !== 'port' && fields[i].kind !== 'choice' && fields[i + 1]?.kind === 'port';
  let i = 0;
  while (i < fields.length) {
    if (withPort(i)) {
      placed.push({ field: fields[i], span: 4 }, { field: fields[i + 1], span: 2 });
      i += 2;
      continue;
    }
    let end = i;
    while (end < fields.length && shortField(fields[end]) && !withPort(end)) end++;
    const run = end - i;
    if (run < 2) {
      placed.push({ field: fields[i], span: 6 });
      i++;
      continue;
    }
    // Rows of three; a rest of one takes one from the row before it (two rows of two).
    let left = run;
    while (left > 0) {
      const row = left === 4 || left === 2 ? 2 : 3;
      for (let k = 0; k < row; k++) placed.push({ field: fields[i++], span: row === 3 ? 2 : 3 });
      left -= row;
    }
  }
  return placed;
}

const sourceKey = (source: CardSource) => `source:${source.host_id}:${source.id}`;

const byName = (a: CardSource, b: CardSource) =>
  a.name.localeCompare(b.name) || a.id.localeCompare(b.id) || a.host_id.localeCompare(b.host_id);

const sourceMethod = <H extends CardHost, S extends CardSource>(source: S, label: (source: S) => string): Method<H, S> => ({
  key: sourceKey(source),
  source,
  label: label(source),
  online: !!source.online,
});

/**
 * The cards: the Lunaris computers in the order given, each with the sources
 * that are that computer; then the other sources by name, those of one group
 * on one card (named after the first of them).
 *
 * `label` names a source's method (its type, translated by the caller).
 */
export function buildCards<H extends CardHost, S extends CardSource>(
  hosts: H[],
  sources: S[],
  label: (source: S) => string = (source) => typeShortName(source.connector, source.connector_name),
  lunarisLabel = 'Lunaris',
): Card<H, S>[] {
  const sorted = [...sources].sort(byName);
  const groupOf = (source: S) => (validGroup(source.group) ? source.group : '');
  const cards: Card<H, S>[] = [];
  const taken = new Set<S>();

  for (const host of hosts) {
    const group = HOST_GROUP + host.id;
    const mine = sorted.filter((source) => groupOf(source) === group);
    mine.forEach((source) => taken.add(source));
    cards.push({
      key: `host:${host.id}`,
      title: host.name,
      host,
      // A computer whose id cannot be part of a group takes no new methods.
      group: validGroup(group) ? group : '',
      methods: [
        { key: 'lunaris', host, label: lunarisLabel, online: host.status === 'Online' } as Method<H, S>,
        ...mine.map((source) => sourceMethod<H, S>(source, label)),
      ],
    });
  }

  const byGroup = new Map<string, Card<H, S>>();
  for (const source of sorted) {
    if (taken.has(source)) continue;
    const group = groupOf(source);
    const known = group ? byGroup.get(group) : undefined;
    if (known) {
      known.methods.push(sourceMethod<H, S>(source, label));
      continue;
    }
    const card: Card<H, S> = {
      key: group ? `group:${group}` : sourceKey(source),
      title: source.name,
      group,
      methods: [sourceMethod<H, S>(source, label)],
    };
    if (group) byGroup.set(group, card);
    cards.push(card);
  }

  // Two methods of one type on a card are told apart by their names.
  for (const card of cards) {
    const count = new Map<string, number>();
    card.methods.forEach((method) => { if (method.source) count.set(method.label, (count.get(method.label) || 0) + 1); });
    for (const method of card.methods) {
      if (method.source && (count.get(method.label) || 0) > 1) method.label = `${method.label}: ${method.source.name}`;
    }
  }
  return cards;
}

/**
 * The method a card connects with: the one remembered for it when it is
 * still there and can be connected to; else the Lunaris computer itself, or
 * the first method, when that can be; else the first that can be; else (the
 * whole card is offline) the remembered one or the first.
 */
export function currentMethod<M extends { key: string; online: boolean }>(methods: M[], remembered?: string | null): M | undefined {
  if (methods.length === 0) return undefined;
  const kept = remembered ? methods.find((method) => method.key === remembered) : undefined;
  if (kept?.online) return kept;
  const first = methods[0];
  if (!kept && first.online) return first;
  return methods.find((method) => method.online) || kept || first;
}

// ---- The method used last, for each card ----

export const METHODS_STORE = 'lunaris_card_methods';
/** The most cards remembered (the oldest are forgotten). */
const MAX_REMEMBERED = 200;

type KeyValue = { getItem(key: string): string | null; setItem(key: string, value: string): void };

/** { "<card key>": "<method key>" }, as it was saved; {} when nothing or nonsense was. */
export function loadMethods(store: KeyValue): Record<string, string> {
  try {
    const parsed: unknown = JSON.parse(store.getItem(METHODS_STORE) || '{}');
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    const out: Record<string, string> = {};
    for (const [card, method] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof method === 'string' && method.length <= 300 && card.length <= 300) out[card] = method;
    }
    return out;
  } catch {
    return {};
  }
}

/** Remembers `method` for `card` and returns the new map. */
export function rememberMethod(store: KeyValue, known: Record<string, string>, card: string, method: string): Record<string, string> {
  const next: Record<string, string> = { ...known };
  delete next[card];
  next[card] = method;
  const keys = Object.keys(next);
  for (const old of keys.slice(0, Math.max(0, keys.length - MAX_REMEMBERED))) delete next[old];
  try { store.setItem(METHODS_STORE, JSON.stringify(next)); } catch { /* storage unavailable */ }
  return next;
}
