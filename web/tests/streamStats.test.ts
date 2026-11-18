// What the stats overlay says (src/streamStats.ts), case by case.
// Run: npm run test:stats   (node --test, no packages needed)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  agentRoundTripMs,
  pingStatText,
  decodeStatText,
  decoderName,
  encodeStatText,
  shortGpuName,
  shownPingMs,
  sourceLatencyMs,
} from '../src/streamStats.ts';

test('the delay to a computer shown through a connector', () => {
  assert.equal(sourceLatencyMs(JSON.parse('{"type":"source_latency","ms":12}')), 12);
  assert.equal(sourceLatencyMs({ type: 'source_latency', ms: 0 }), 0);
  // The other messages of the channel, and what is not a count of milliseconds.
  assert.equal(sourceLatencyMs({ type: 'host_cursor', x: 1, y: 2 }), null);
  assert.equal(sourceLatencyMs({ type: 'source_displays', displays: [] }), null);
  assert.equal(sourceLatencyMs({ type: 'source_latency' }), null);
  assert.equal(sourceLatencyMs({ type: 'source_latency', ms: '12' }), null);
  assert.equal(sourceLatencyMs({ type: 'source_latency', ms: -3 }), null);
  assert.equal(sourceLatencyMs(null), null);
  assert.equal(sourceLatencyMs('source_latency'), null);
});

test('the round trip to the agent is the one of the pair in use', () => {
  const used = { type: 'candidate-pair', id: 'P2', nominated: true, state: 'succeeded', currentRoundTripTime: 0.0042 };
  const tried = { type: 'candidate-pair', id: 'P1', state: 'succeeded', currentRoundTripTime: 0.087 };
  const later = { type: 'candidate-pair', id: 'P3', state: 'waiting', currentRoundTripTime: 0.25 };
  // By the transport's word, wherever the pair stands among the reports.
  const transport = { type: 'transport', selectedCandidatePairId: 'P2' };
  assert.ok(Math.abs(agentRoundTripMs([tried, used, later, transport])! - 4.2) < 1e-9);
  assert.ok(Math.abs(agentRoundTripMs([transport, later, used, tried])! - 4.2) < 1e-9);
  // Without a transport report: the pair marked selected, else the nominated one that succeeded.
  assert.ok(Math.abs(agentRoundTripMs([tried, { ...used, nominated: false, selected: true }, later])! - 4.2) < 1e-9);
  assert.ok(Math.abs(agentRoundTripMs([tried, used, later])! - 4.2) < 1e-9);
  // No current figure on the pair: the mean of its checks.
  assert.ok(Math.abs(agentRoundTripMs([{ type: 'candidate-pair', id: 'P2', selected: true, totalRoundTripTime: 0.06, responsesReceived: 20 }])! - 3) < 1e-9);
  // No pair with a figure: what RTCP says of a stream this side sends.
  assert.ok(Math.abs(agentRoundTripMs([{ type: 'remote-inbound-rtp', roundTripTime: 0.011 }])! - 11) < 1e-9);
  // Nothing.
  assert.equal(agentRoundTripMs([{ type: 'inbound-rtp', kind: 'video' }, tried]), null);
  assert.equal(agentRoundTripMs([]), null);
});

test('the ping row shows tenths under ten milliseconds', () => {
  assert.equal(pingStatText(0.44), '0.4 ms');
  assert.equal(pingStatText(9.6), '9.6 ms');
  assert.equal(pingStatText(9.97), '10 ms');
  assert.equal(pingStatText(12.4), '12 ms');
  assert.equal(pingStatText(0), '0.0 ms');
});

test('the ping is the whole way', () => {
  assert.equal(sourceLatencyMs({ type: 'source_latency', ms: 0.4 }), 0.4);
  assert.equal(shownPingMs(0.1, 12), 12.1);
  // The agent's own screen: its ping as before.
  assert.equal(shownPingMs(7.5, 0), 7.5);
});

test('GPU names are shortened', () => {
  assert.equal(shortGpuName('NVIDIA GeForce RTX 2060'), 'RTX 2060');
  assert.equal(shortGpuName('NVIDIA GeForce RTX 2060 (vendor=0x10de, device=0x1f08)'), 'RTX 2060');
  assert.equal(shortGpuName('AMD Radeon RX 6600/6600 XT/6600M'), 'RX 6600/6600 XT/6600M');
  assert.equal(shortGpuName('Intel(R) UHD Graphics 630 (vendor=0x8086, device=0x3e92)'), 'UHD Graphics 630');
  assert.equal(shortGpuName('AMD GPU (amdgpu, vendor=0x1002, device=0x73ff)'), 'AMD GPU');
  assert.equal(shortGpuName('Unknown GPU (vendor=0x1234, device=0x0001)'), '');
  assert.equal(shortGpuName('Unknown (encoded on the remote computer)'), '');
  assert.equal(shortGpuName(''), '');
  assert.equal(shortGpuName(undefined), '');
});

test('the Encode row', () => {
  assert.equal(encodeStatText('h264_nvenc', 'NVENC', 'NVIDIA GeForce RTX 2060'), 'h264_nvenc · RTX 2060');
  assert.equal(encodeStatText('libx264', 'Software', 'NVIDIA GeForce RTX 2060'), 'libx264 · CPU');
  assert.equal(encodeStatText('h264_amf', 'AMF', ''), 'h264_amf');
  // A session that shows another computer through a connector.
  assert.equal(encodeStatText('remote', 'connector', 'Unknown (encoded on the remote computer)'), 'Remote computer');
  // Before the host said anything.
  assert.equal(encodeStatText('Pending', 'Unknown', ''), '-');
  assert.equal(encodeStatText('Unknown', 'Unknown', ''), '-');
});

test('the Decode row', () => {
  assert.equal(decoderName('VDAVideoDecoder', true), 'Hardware (VDAVideoDecoder)');
  assert.equal(decoderName('libdav1d', false), 'Software (libdav1d)');
  assert.equal(decoderName('ExternalDecoder (D3D11VideoDecoder)', true), 'Hardware (D3D11VideoDecoder)');
  // Only one of the two.
  assert.equal(decoderName('FFmpeg', undefined), 'FFmpeg');
  assert.equal(decoderName(undefined, true), 'Hardware');
  assert.equal(decoderName('unknown', false), 'Software');
  // Neither: only the time.
  assert.equal(decoderName(undefined, undefined), '');
  assert.equal(decodeStatText('', 5.44), '5.4 ms');
  assert.equal(decodeStatText('Hardware (VDAVideoDecoder)', 5.44), 'Hardware (VDAVideoDecoder) · 5.4 ms');
  assert.equal(decodeStatText('Software (libdav1d)', 6), 'Software (libdav1d) · 6.0 ms');
});
