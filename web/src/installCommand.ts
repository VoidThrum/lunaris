// One-line agent installation: the commands an administrator pastes into a
// terminal, and the state of each install token. Pure functions only, so they
// can be tested with node --test (tests/installCommand.test.ts).

export type InstallOs = 'linux' | 'windows';

export interface InstallOptions {
  /** Computer name; empty or missing means the agent uses the hostname. */
  name?: string;
  /** Skip the certificate check when downloading (self-signed server). */
  insecure?: boolean;
}

/** Quotes a value for bash inside single quotes ('it'\''s'). */
export const bashQuote = (value: string): string =>
  `'${value.replace(/'/g, `'\\''`)}'`;

/** Quotes a value for PowerShell inside single quotes ('it''s'). PowerShell
 *  also treats the typographic single quotes as quote characters, so those
 *  are doubled as well. */
export const psQuote = (value: string): string =>
  `'${value.replace(/['‘’‚‛]/g, (q) => q + q)}'`;

/** A URL without characters any shell would treat specially is left bare
 *  (easier to read); anything else gets quoted. */
const urlWord = (url: string, quote: (v: string) => string): string =>
  /^[A-Za-z0-9_.:/%@+=,~-]+$/.test(url) ? url : quote(url);

/** The server origin without a trailing slash, e.g. "https://lunaris.example". */
export const normalizeOrigin = (origin: string): string => origin.trim().replace(/\/+$/, '');

export const installScriptUrl = (os: InstallOs, origin: string): string =>
  `${normalizeOrigin(origin)}/install/${os === 'linux' ? 'agent.sh' : 'agent.ps1'}`;

/** The command to paste into a terminal: a root shell on Linux, PowerShell
 *  run as Administrator on Windows. */
export const installCommand = (
  os: InstallOs,
  origin: string,
  token: string,
  opts: InstallOptions = {},
): string => {
  const name = (opts.name ?? '').trim();
  const url = installScriptUrl(os, origin);
  if (os === 'linux') {
    const curl = opts.insecure ? 'curl -fsSLk' : 'curl -fsSL';
    let cmd = `${curl} ${urlWord(url, bashQuote)} | sudo LUNARIS_INSTALL_TOKEN=${bashQuote(token)} bash -s --`;
    if (name) cmd += ` --name ${bashQuote(name)}`;
    return cmd;
  }
  // Windows PowerShell 5.1 has no switch to skip the certificate check for
  // irm, so `insecure` changes nothing here; the page explains the options.
  let cmd = `$env:LUNARIS_INSTALL_TOKEN=${psQuote(token)}; & ([scriptblock]::Create((irm ${urlWord(url, psQuote)})))`;
  if (name) cmd += ` -Name ${psQuote(name)}`;
  return cmd;
};

// ---- Tokens ---------------------------------------------------------------

export interface InstallToken {
  id: string;
  label: string;
  created_at: string | number | null;
  expires_at: string | number | null;
  max_uses: number | null;
  uses: number;
  revoked: boolean;
}

export type TokenStatus = 'active' | 'expired' | 'used_up' | 'revoked';

/** Milliseconds since the epoch, or null. Accepts unix seconds (or ms),
 *  ISO 8601 strings, and SQLite's "YYYY-MM-DD HH:MM:SS" which is UTC. */
export const parseTimestamp = (value: unknown): number | null => {
  if (value === null || value === undefined || value === '') return null;
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) return null;
    return value < 1e12 ? value * 1000 : value; // seconds vs milliseconds
  }
  if (typeof value !== 'string') return null;
  const s = value.trim();
  if (/^\d+(\.\d+)?$/.test(s)) return parseTimestamp(Number(s));
  // A date and time without a zone: the server stores UTC.
  const m = /^(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}(:\d{2}(\.\d+)?)?)$/.exec(s);
  const ms = Date.parse(m ? `${m[1]}T${m[2]}Z` : s);
  return Number.isNaN(ms) ? null : ms;
};

/** Revoked wins, then expired, then used up. */
export const tokenStatus = (tok: Pick<InstallToken, 'expires_at' | 'max_uses' | 'uses' | 'revoked'>, now: number = Date.now()): TokenStatus => {
  if (tok.revoked) return 'revoked';
  const exp = parseTimestamp(tok.expires_at);
  if (exp !== null && exp <= now) return 'expired';
  if (tok.max_uses !== null && tok.max_uses !== undefined && tok.uses >= tok.max_uses) return 'used_up';
  return 'active';
};

/** "Valid for" choices in the create form, in hours. */
export const VALIDITY_CHOICES: { hours: number; label: string }[] = [
  { hours: 1, label: '1 hour' },
  { hours: 24, label: '24 hours' },
  { hours: 24 * 7, label: '7 days' },
  { hours: 24 * 30, label: '30 days' },
];

/** The "max computers" field: empty means unlimited (null); otherwise a
 *  whole number of at least 1, or undefined when it is not valid. */
export const parseMaxUses = (text: string): number | null | undefined => {
  const s = text.trim();
  if (!s) return null;
  if (!/^\d+$/.test(s)) return undefined;
  const n = Number(s);
  return n >= 1 && Number.isSafeInteger(n) ? n : undefined;
};

/** A rough relative time: [amount, unit], unit one of minute/hour/day, and
 *  the sign tells past (<0) from future (>0). Under a minute gives 0. */
export const relativeParts = (target: number, now: number = Date.now()): { amount: number; unit: 'minute' | 'hour' | 'day'; future: boolean } => {
  const diff = target - now;
  const abs = Math.abs(diff);
  const min = 60_000, hour = 60 * min, day = 24 * hour;
  if (abs >= day) return { amount: Math.round(abs / day), unit: 'day', future: diff > 0 };
  if (abs >= hour) return { amount: Math.round(abs / hour), unit: 'hour', future: diff > 0 };
  return { amount: Math.round(abs / min), unit: 'minute', future: diff > 0 };
};
