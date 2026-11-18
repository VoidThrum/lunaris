// One card for each computer (src/computerCards.ts), case by case.
// Run: npm run test:cards   (node --test, no packages needed)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  buildCards,
  currentMethod,
  loadMethods,
  newGroup,
  rememberMethod,
  layoutFields,
  shortTypeName,
  typeShortName,
  validGroup,
  METHODS_STORE,
  type CardHost,
  type CardSource,
} from '../src/computerCards.ts';

const host = (id: string, name: string, status = 'Online'): CardHost => ({ id, name, status });
const source = (id: string, name: string, connector: string, over: Partial<CardSource> = {}): CardSource => ({
  host_id: 'h1',
  host_name: 'Office PC',
  id,
  name,
  connector,
  connector_name: connector === 'rustdesk' ? 'RustDesk (by IP address)' : connector === 'sunshine' ? 'Sunshine / GameStream' : connector,
  online: true,
  ...over,
});

const shape = (cards: ReturnType<typeof buildCards>) =>
  cards.map((card) => `${card.key} "${card.title}" [${card.methods.map((m) => `${m.label}${m.online ? '' : ' (off)'}`).join(', ')}]`);

test('sources without a group are cards of their own, as before', () => {
  const cards = buildCards(
    [host('h1', 'Office PC'), host('h2', 'Lab PC', 'Offline')],
    [source('zed-1', 'Zed', 'rustdesk'), source('den-1', 'Den PC', 'sunshine', { online: false })],
  );
  assert.deepEqual(shape(cards), [
    'host:h1 "Office PC" [Lunaris]',
    'host:h2 "Lab PC" [Lunaris (off)]',
    'source:h1:den-1 "Den PC" [Sunshine (off)]',
    'source:h1:zed-1 "Zed" [RustDesk]',
  ]);
  assert.equal(cards[2].group, '');
  assert.equal(cards[0].group, 'host:h1');
});

test('sources of one group are one card, named after the first by name', () => {
  const cards = buildCards(
    [host('h1', 'Office PC')],
    [
      source('den-sun', 'Den PC (Sunshine)', 'sunshine', { group: 'g-1' }),
      source('alone', 'Alone', 'rustdesk'),
      source('den-rd', 'Den PC', 'rustdesk', { group: 'g-1' }),
      // Through another Lunaris computer: the same group is the same computer.
      source('den-x', 'Den PC X', 'acme', { group: 'g-1', host_id: 'h2', host_name: 'Lab PC', online: false }),
    ],
  );
  assert.deepEqual(shape(cards), [
    'host:h1 "Office PC" [Lunaris]',
    'source:h1:alone "Alone" [RustDesk]',
    'group:g-1 "Den PC" [RustDesk, Sunshine, acme (off)]',
  ]);
  assert.deepEqual(cards[2].methods.map((m) => m.key), ['source:h1:den-rd', 'source:h1:den-sun', 'source:h2:den-x']);
  assert.equal(cards[2].group, 'g-1');
  // The same lists in another order give the same cards.
  const again = buildCards([host('h1', 'Office PC')], [
    source('den-x', 'Den PC X', 'acme', { group: 'g-1', host_id: 'h2', host_name: 'Lab PC', online: false }),
    source('den-rd', 'Den PC', 'rustdesk', { group: 'g-1' }),
    source('alone', 'Alone', 'rustdesk'),
    source('den-sun', 'Den PC (Sunshine)', 'sunshine', { group: 'g-1' }),
  ]);
  assert.deepEqual(shape(again), shape(cards));
});

test('a Lunaris computer takes in the sources that are that computer', () => {
  const cards = buildCards(
    [host('h1', 'Office PC'), host('h2', 'Lab PC', 'Busy')],
    [
      source('office-rd', 'Office (RustDesk)', 'rustdesk', { group: 'host:h1', host_id: 'h2', host_name: 'Lab PC' }),
      source('office-sun', 'Office (Sunshine)', 'sunshine', { group: 'host:h1', online: false }),
      source('lab-rd', 'Lab by RustDesk', 'rustdesk', { group: 'host:h2' }),
    ],
  );
  assert.deepEqual(shape(cards), [
    'host:h1 "Office PC" [Lunaris, RustDesk, Sunshine (off)]',
    // Busy: the Lunaris way cannot be connected to now, RustDesk can.
    'host:h2 "Lab PC" [Lunaris (off), RustDesk]',
  ]);
  assert.equal(cards[0].methods[0].key, 'lunaris');
  assert.equal(cards[0].host?.id, 'h1');
});

test('a source that names a Lunaris computer this person does not have stays its own card', () => {
  // The server leaves the group out for a computer the person may not use;
  // one that is gone (or a server that did not) is handled the same way here.
  const cards = buildCards(
    [host('h1', 'Office PC')],
    [
      source('a', 'A', 'rustdesk', { group: 'host:h9' }),
      source('b', 'B', 'sunshine', { group: 'host:h9' }),
      source('c', 'C', 'rustdesk'),
    ],
  );
  assert.deepEqual(shape(cards), [
    'host:h1 "Office PC" [Lunaris]',
    'group:host:h9 "A" [RustDesk, Sunshine]',
    'source:h1:c "C" [RustDesk]',
  ]);
  assert.ok(!cards.some((card) => card.host?.id === 'h9'));
});

test('a group that is not one is no group', () => {
  for (const bad of ['', ' ', 'a b', '<img src=x onerror=alert(1)>', 'x'.repeat(65), 'nhóm', "a'b"]) {
    assert.equal(validGroup(bad), false, bad);
    const cards = buildCards([], [source('a', 'A', 'rustdesk', { group: bad }), source('b', 'B', 'sunshine', { group: bad })]);
    assert.deepEqual(cards.map((card) => card.key), ['source:h1:a', 'source:h1:b']);
  }
  assert.equal(validGroup(undefined), false);
  assert.equal(validGroup(7), false);
  assert.ok(validGroup('host:0b7c1f0e-3d7a-4c57-9a0e-6f0f4c1d2e3f'));
  const made = newGroup();
  assert.match(made, /^g-[0-9a-f]{12}$/);
  assert.ok(validGroup(made));
  assert.notEqual(newGroup(), newGroup());
});

test('two methods of one type on a card are told apart by name', () => {
  const cards = buildCards([], [
    source('a', 'Den (admin)', 'rustdesk', { group: 'g-1' }),
    source('b', 'Den (guest)', 'rustdesk', { group: 'g-1' }),
    source('c', 'Den', 'sunshine', { group: 'g-1' }),
  ]);
  assert.deepEqual(cards[0].methods.map((m) => m.label), ['Sunshine', 'RustDesk: Den (admin)', 'RustDesk: Den (guest)']);
  assert.equal(shortTypeName('RustDesk (by IP address)'), 'RustDesk');
  assert.equal(shortTypeName('(odd)'), '(odd)');
});

test('the method a card connects with', () => {
  const m = (key: string, online = true) => ({ key, online });
  assert.equal(currentMethod([]), undefined);
  // Nothing remembered: the Lunaris computer itself when there is one (it is first), else the first.
  assert.equal(currentMethod([m('lunaris'), m('source:h1:a')])?.key, 'lunaris');
  assert.equal(currentMethod([m('source:h1:a'), m('source:h1:b')])?.key, 'source:h1:a');
  // The one used last.
  assert.equal(currentMethod([m('lunaris'), m('source:h1:a')], 'source:h1:a')?.key, 'source:h1:a');
  // ... unless it is gone, or cannot be connected to now while another can.
  assert.equal(currentMethod([m('lunaris'), m('source:h1:a')], 'source:h1:gone')?.key, 'lunaris');
  assert.equal(currentMethod([m('lunaris'), m('source:h1:a', false)], 'source:h1:a')?.key, 'lunaris');
  assert.equal(currentMethod([m('lunaris', false), m('source:h1:a'), m('source:h1:b')])?.key, 'source:h1:a');
  // Nothing can be connected to: what it would be.
  assert.equal(currentMethod([m('lunaris', false), m('source:h1:a', false)], 'source:h1:a')?.key, 'source:h1:a');
  assert.equal(currentMethod([m('lunaris', false), m('source:h1:a', false)])?.key, 'lunaris');
});

test('the method used last is remembered for each card', () => {
  const kept = new Map<string, string>();
  const store = { getItem: (key: string) => kept.get(key) ?? null, setItem: (key: string, value: string) => { kept.set(key, value); } };
  assert.deepEqual(loadMethods(store), {});
  let known = rememberMethod(store, {}, 'host:h1', 'source:h1:a');
  known = rememberMethod(store, known, 'group:g-1', 'source:h1:b');
  known = rememberMethod(store, known, 'host:h1', 'lunaris');
  assert.deepEqual(known, { 'group:g-1': 'source:h1:b', 'host:h1': 'lunaris' });
  assert.deepEqual(loadMethods(store), { 'group:g-1': 'source:h1:b', 'host:h1': 'lunaris' });
  // Nonsense in the store is nothing.
  for (const junk of ['not json', '[1,2]', 'null', '{"a":1,"b":{"c":2}}']) {
    kept.set(METHODS_STORE, junk);
    assert.deepEqual(loadMethods(store), {});
  }
  // A store that cannot be written to: the choice still holds for this page.
  const readOnly = { getItem: () => null, setItem: () => { throw new Error('full'); } };
  assert.deepEqual(rememberMethod(readOnly, {}, 'host:h1', 'lunaris'), { 'host:h1': 'lunaris' });
  // Not without end.
  let many: Record<string, string> = {};
  for (let i = 0; i < 250; i++) many = rememberMethod(store, many, `card-${i}`, 'lunaris');
  assert.equal(Object.keys(many).length, 200);
  assert.ok(!('card-0' in many) && 'card-249' in many);
});

test('the short name of a type', () => {
  assert.equal(typeShortName('rustdesk', 'RustDesk (by IP address)'), 'RustDesk');
  assert.equal(typeShortName('sunshine', 'Sunshine / GameStream'), 'Sunshine');
  assert.equal(typeShortName('lunaris', 'Lunaris'), 'Lunaris');
  // A type that is not known here: its name without its brackets, whole.
  assert.equal(typeShortName('acme-view', 'Acme Viewer (beta)'), 'Acme Viewer');
  assert.equal(typeShortName('template', ''), 'template');
  // Not a property of every object.
  assert.equal(typeShortName('constructor', 'Odd'), 'Odd');
});

test('which fields of a form share a row', () => {
  const f = (key: string, kind = 'text', def = '') => ({ key, kind, default: def });
  const rows = (fields: ReturnType<typeof f>[]) => layoutFields(fields).map((p) => `${p.field.key}:${p.span}`).join(' ');
  // RustDesk: the address with its port, the password alone.
  assert.equal(rows([f('host'), f('port', 'port', '21118'), f('password', 'secret')]), 'host:4 port:2 password:6');
  // Sunshine: address + port, the three short ones on a row, the choice alone.
  assert.equal(
    rows([f('host'), f('port', 'port', '47989'), f('app', 'text', 'Desktop'), f('resolution', 'text', '1920x1080'), f('bitrate', 'text', '20'), f('pointer', 'choice', 'picture')]),
    'host:4 port:2 app:2 resolution:2 bitrate:2 pointer:6',
  );
  // Long or empty usual values, secrets and choices have a row of their own.
  assert.equal(rows([f('station'), f('gateway', 'text', 'gw.example.net'), f('gateway_port', 'port', '7400'), f('token', 'secret'), f('pin', 'secret')]),
    'station:6 gateway:4 gateway_port:2 token:6 pin:6');
  assert.equal(rows([f('a', 'choice', 'x'), f('p', 'port', '1')]), 'a:6 p:6');
  assert.equal(rows([]), '');
  // Short ones in a row: three across, and never one left alone.
  const short = (n: number) => Array.from({ length: n }, (_, i) => f('s' + i, 'text', 'v'));
  assert.equal(rows(short(1)), 's0:6');
  assert.equal(rows(short(2)), 's0:3 s1:3');
  assert.equal(rows(short(4)), 's0:3 s1:3 s2:3 s3:3');
  assert.equal(rows(short(5)), 's0:2 s1:2 s2:2 s3:3 s4:3');
  assert.equal(rows(short(7)), 's0:2 s1:2 s2:2 s3:3 s4:3 s5:3 s6:3');
  // A short one before a port is that port's pair, not the last of a row.
  assert.equal(rows([f('a', 'text', '1'), f('b', 'text', '2'), f('p', 'port', '3')]), 'a:6 b:4 p:2');
  assert.equal(rows([f('p', 'port', '1'), f('q', 'port', '2')]), 'p:3 q:3');
  // Every field once, in its order.
  const many = [f('a', 'text', '1'), f('b', 'text', '2'), f('c', 'text', '3'), f('d'), f('e', 'port', '5')];
  assert.deepEqual(layoutFields(many).map((p) => p.field.key), ['a', 'b', 'c', 'd', 'e']);
});
