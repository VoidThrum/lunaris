// What the stats overlay of the stream view says: the ping with the way to
// a computer shown through a connector, and the "Encode" and "Decode" rows.
// Pure: tests/streamStats.test.ts.

/**
 * The delay between the agent and the computer it shows, there and back, in
 * milliseconds, when `message` is the one that tells it on the general data
 * channel (`{"type":"source_latency","ms":12}`, agent connector/media.rs
 * `latency_message`); else null. Only sessions that show another computer
 * through a connector send it.
 */
// (`ms` is a number with a fraction: the agent tells tenths of a millisecond.)
export function sourceLatencyMs(message: unknown): number | null {
  if (!message || typeof message !== 'object') return null;
  const m = message as { type?: unknown; ms?: unknown };
  if (m.type !== 'source_latency') return null;
  if (typeof m.ms !== 'number' || !Number.isFinite(m.ms) || m.ms < 0) return null;
  return m.ms;
}

/**
 * The ping shown: the viewer's round trip to the agent plus the agent's to
 * the computer it shows (0 when the agent shows its own screen).
 */
export function shownPingMs(agentRoundTripMs: number, sourceLatency: number): number {
  return agentRoundTripMs + sourceLatency;
}

/**
 * A GPU's name as the host describes it, short: "RTX 2060" of
 * "NVIDIA GeForce RTX 2060 (vendor=0x10de, device=0x1f08)". "" when the host
 * named none ("Unknown ...").
 */
export function shortGpuName(info: string | null | undefined): string {
  let s = String(info ?? '').trim();
  if (s === '' || /^unknown/i.test(s)) return '';
  // "(vendor=0x.., device=0x..)", "(amdgpu, vendor=.., device=..)"
  s = s.replace(/\s*\([^()]*vendor=[^()]*\)\s*$/i, '');
  s = s.replace(/\((R|TM)\)/gi, '').replace(/\s+/g, ' ').trim();
  const model = s
    .replace(/^(NVIDIA|AMD|ATI|Intel)( Corporation)?\s+/i, '')
    .replace(/^(GeForce|Radeon)\s+/i, '');
  // "AMD GPU": nothing but the maker is known, so the maker stays.
  return model === '' || /^GPU$/i.test(model) ? s : model;
}

/**
 * What encodes the picture: "h264_nvenc · RTX 2060". Of a session that shows
 * another computer through a connector (the agent then says encoder
 * "remote", type "connector": the picture is encoded there, by what nobody
 * tells) "Remote computer". "-" before the host said anything.
 */
export function encodeStatText(encoder: string, hwType: string, gpuInfo: string): string {
  const name = String(encoder ?? '');
  const type = String(hwType ?? '').toLowerCase();
  if (type === 'connector' || name.toLowerCase() === 'remote') return 'Remote computer';
  if (name === '' || name === 'Unknown' || name === 'Pending') return '-';
  // A software encoder runs on the processor, whatever GPU the host has.
  if (type === 'software') return `${name} · CPU`;
  const gpu = shortGpuName(gpuInfo);
  return gpu !== '' ? `${name} · ${gpu}` : name;
}

/**
 * What decodes the picture, from the browser's inbound video statistics
 * (W3C webrtc-stats, RTCInboundRtpStreamStats): `decoderImplementation` (the
 * decoder's name) and `powerEfficientDecoder` (true = hardware). "" when the
 * browser gives neither. A name that wraps another in brackets is cut to
 * the inner one, so that brackets do not nest.
 */
export function decoderName(implementation: unknown, powerEfficient: unknown): string {
  let name = typeof implementation === 'string' ? implementation.trim() : '';
  if (/^unknown$/i.test(name)) name = '';
  const inner = /^[^()]+\(([^()]+)\)$/.exec(name);
  if (inner) name = inner[1].trim();
  const kind = powerEfficient === true ? 'Hardware' : powerEfficient === false ? 'Software' : '';
  if (kind && name) return `${kind} (${name})`;
  return kind || name;
}

/** The "Decode" row: "Hardware (VDAVideoDecoder) · 5.4 ms", or only the time. */
export function decodeStatText(decoder: string, ms: number): string {
  const time = `${ms.toFixed(1)} ms`;
  return decoder ? `${decoder} · ${time}` : time;
}

/**
 * The viewer's round trip to the agent in milliseconds, from the reports of
 * `RTCPeerConnection.getStats()`; null when they hold none.
 *
 * W3C webrtc-stats: of the candidate pair in use
 * (`RTCTransportStats.selectedCandidatePairId`; browsers without it mark the
 * pair `selected`, or it is the nominated pair that succeeded),
 * `currentRoundTripTime` is "the latest round trip time measured in seconds,
 * computed from both STUN connectivity checks, including those that are sent
 * for consent verification". That is the figure taken: one timed request
 * and answer on the very path the picture takes, refreshed whenever the
 * browser checks the pair. Other pairs (tried and not used) are not looked
 * at. Without it, `remote-inbound-rtp.roundTripTime` (from the RTCP reports
 * about a stream this side sends, the microphone; seconds as well).
 */
export function agentRoundTripMs(reports: Iterable<any>): number | null {
  const all = Array.from(reports);
  const selectedIds = new Set(
    all.filter(r => r?.type === 'transport' && r.selectedCandidatePairId).map(r => r.selectedCandidatePairId),
  );
  const pairs = all.filter(r => r?.type === 'candidate-pair');
  const pair =
    pairs.find(r => selectedIds.has(r.id)) ??
    pairs.find(r => r.selected === true) ??
    pairs.find(r => r.nominated === true && r.state === 'succeeded');
  if (pair) {
    const current = Number(pair.currentRoundTripTime);
    if (Number.isFinite(current) && current > 0) return current * 1000;
    const responses = Number(pair.responsesReceived);
    const total = Number(pair.totalRoundTripTime);
    if (responses > 0 && Number.isFinite(total) && total > 0) return (total / responses) * 1000;
  }
  for (const report of all) {
    if (report?.type !== 'remote-inbound-rtp') continue;
    const rtt = Number(report.roundTripTime);
    if (Number.isFinite(rtt) && rtt > 0) return rtt * 1000;
  }
  return null;
}

/**
 * The "Ping" row: tenths of a millisecond while it is under ten, whole
 * milliseconds from there ("0.4 ms", "9.6 ms", "12 ms").
 */
export function pingStatText(ms: number): string {
  return `${ms < 9.95 ? ms.toFixed(1) : ms.toFixed(0)} ms`;
}
