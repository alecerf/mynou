// Words and numbers as a person reads them: labels, sizes, dates, media names.

export const TERMINAL_STATES = new Set(['ready', 'failed', 'cancelled']);
// States in which a worker is busy with the job right now.
export const WORKING = new Set(['processing', 'importing']);

// One label table: internal values never reach the page as they are.
const LABELS = {
  job: {
    queued: 'Waiting', processing: 'Looking for a release', downloading: 'Downloading', importing: 'Adding to the library',
    imported: 'Imported', ready: 'In your library', failed: 'Failed', cancelled: 'Cancelled',
  },
  transfer: {
    queued: 'Queued', downloading: 'Downloading', verifying: 'Verifying', selected_ready: 'Selected files ready', ready: 'Complete',
    queued_seed: 'Waiting to seed', seed_limited: 'Seeding goal reached', paused: 'Paused', failed: 'Failed',
  },
  kind: { movie: 'Film', episode: 'Episode', series: 'Series' },
  priority: { low: 'Low priority', normal: 'In progress', high: 'High priority' },
  upgrade: {
    same_acquisition: 'Already the chosen release', upgrade_available: 'Upgrade available', entry_changed: 'Changed during the check',
    upgrade_pending: 'Upgrade on its way', imports_missing: 'File missing', cutoff_reached: 'Quality goal reached',
    search_unavailable: 'Search unavailable', no_improvement: 'No better release', shared_group_upgrade_required: 'Upgrade as a group',
  },
};
// Words that keep their capitals when a field name becomes a label.
const ACRONYMS = { id: 'ID', url: 'URL', api: 'API', irc: 'IRC', tmdb: 'TMDB', tls: 'TLS', rss: 'RSS', ip: 'IP', vpn: 'VPN', secs: 'seconds', bps: 'bytes/s' };
export const sentence = (value) => {
  const words = String(value ?? '').split('_').filter(Boolean).map((w) => ACRONYMS[w.toLowerCase()] ?? w.toLowerCase());
  return words.length ? words.join(' ').replace(/^[a-z]/, (c) => c.toUpperCase()) : '';
};
export const label = (table, value) => LABELS[table][value] ?? sentence(value);
export const formatBytes = (value) => {
  let n = Number(value);
  if (!Number.isFinite(n)) return '–';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 10 ? 1 : 0)} ${units[i]}`;
};

/** Milliseconds for a time the API sends as Unix seconds (number or string) or ISO text; NaN otherwise. */
export const timeOf = (value) => {
  if (value === null || value === undefined || value === '' || value === 0 || value === '0') return NaN;
  const n = Number(value);
  return Number.isFinite(n) ? n * 1000 : Date.parse(value);
};

export const relativeTime = (() => {
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'short' });
  return (value) => {
    const t = timeOf(value);
    if (Number.isNaN(t)) return '';
    const s = Math.round((t - Date.now()) / 1000);
    for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]]) {
      if (Math.abs(s) >= size) return rtf.format(Math.round(s / size), unit);
    }
    return rtf.format(s, 'second');
  };
})();

export const dateTime = (() => {
  const format = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
  return (value) => { const t = timeOf(value); return Number.isNaN(t) ? '' : format.format(t); };
})();

const toneFor = (state) =>
  state === 'ready' || state === 'imported' ? 'ok' : state === 'failed' || state === 'cancelled' ? 'bad' : 'warn';

export const JOB_ICONS = { queued: 'clock', processing: 'search', downloading: 'down', importing: 'library', imported: 'check', ready: 'check', failed: 'alert', cancelled: 'close' };
export const jobTone = (state) => (state === 'downloading' || state === 'importing' ? 'accent' : toneFor(state));
const hashOf = (text) => {
  let hash = 0;
  for (const char of String(text)) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  return hash;
};
/** A stable tone for a typographic cover, chosen from its title. */
export const toneOf = (text, count = 8) => String(hashOf(text) % count);

/** Quality traits read from a recorded release title. Nothing is guessed beyond its words. */
export function qualityOf(title) {
  const text = String(title ?? '');
  if (!text) return [];
  const pick = (rules) => rules.find(([pattern]) => pattern.test(text))?.[1];
  return [
    pick([[/\b(2160p|4k|uhd)\b/i, '2160p'], [/\b1080[pi]\b/i, '1080p'], [/\b720p\b/i, '720p'], [/\b576p\b/i, '576p'], [/\b480p\b/i, '480p']]),
    pick([[/\b(x265|h\.?265|hevc)\b/i, 'HEVC'], [/\b(x264|h\.?264|avc)\b/i, 'H.264'], [/\bav1\b/i, 'AV1']]),
    pick([[/\bremux\b/i, 'Remux'], [/\b(blu-?ray|bdrip|brrip)\b/i, 'Blu-ray'], [/\bweb-?dl\b/i, 'WEB-DL'], [/\bwebrip\b/i, 'WEBRip'], [/\bhdtv\b/i, 'HDTV']]),
    pick([[/\b(dv|dovi|dolby.?vision)\b/i, 'Dolby Vision'], [/\bhdr(10\+?)?\b/i, 'HDR']]),
  ].filter(Boolean);
}
export const episodeCode = (request) =>
  `S${String(request.season ?? 0).padStart(2, '0')} E${String(request.episode ?? 0).padStart(2, '0')}`;
/** "Title S01 E02" for an episode, "Title (2024)" otherwise. */
export function mediaLabel(request = {}) {
  if (request.kind === 'episode') return `${request.title ?? ''} ${episodeCode(request)}`;
  return `${request.title ?? ''}${Number(request.year) ? ` (${request.year})` : ''}`;
}
/** The line under a title: year, or episode code, and the kind. */
export function mediaLine(request = {}) {
  if (request.kind === 'episode') return episodeCode(request);
  return Number(request.year) ? String(request.year) : label('kind', request.kind);
}
/** "6 min", "1 h 20 min": what is left of a download at its measured speed. */
export const duration = (seconds) => {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return 'under a minute';
  const m = Math.round(s / 60);
  return m < 60 ? `${m} min` : `${Math.floor(m / 60)} h${m % 60 ? ` ${m % 60} min` : ''}`;
};
export const yesNo = (value) => (value ? 'Yes' : 'No');
/** Job progress is a fraction; older records may hold a percentage. */
export const fractionOf = (value) => { const n = Number(value) || 0; return Math.min(1, Math.max(0, n > 1 ? n / 100 : n)); };
