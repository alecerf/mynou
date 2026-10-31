import { formatBytes, relativeTime, mediaLabel, episodeCode, sentence } from './format.js';
import { $, h, reconcile } from './dom.js';
import { data, itemsOf, rateOf } from './store.js';
import { plexError, indexerReports, INDEXER_ERRORS } from './sources.js';

// The overview's statistics. The service computes them in the background, once
// a minute (Plex every ten), and pushes them as the `stats` topic; downloads and
// health come from the live status, transfers and indexer reports.

const count = (value) => (value === undefined || value === null ? '–' : Number(value).toLocaleString());
const VPN_STATES = {
  protected: ['Protected', 'ok'], connecting: ['Connecting', 'warn'], reconnecting: ['Reconnecting', 'warn'],
  disabled: ['Off', ''], blocked: ['Blocked', 'bad'], error: ['Error', 'bad'], external: ['Not managed here', 'warn'],
};

/** Paints a panel's body again only when what it shows changed. */
function paintOnce(el, value, build) {
  const signature = JSON.stringify(value);
  if (el.dataset.signature === signature) return;
  el.dataset.signature = signature;
  el.replaceChildren(...[build()].flat().filter(Boolean));
}

const note = (text, tone) => h('p', { class: 'panel-note', dataset: { tone: tone ?? '' } }, text);
function figures(rows) {
  return h('dl', { class: 'figures' }, rows.filter(Boolean).map(([name, value, tone]) =>
    h('div', { class: 'figure' }, h('dt', {}, name), h('dd', { dataset: { tone: tone ?? '' } }, value))));
}

export function paintStats() {
  const stats = data.get('stats')?.data;
  const status = data.get('status')?.data;
  paintRequests(stats);
  paintWatchlist(stats);
  paintDownloads(status);
  paintHealth(stats, status);
  paintSeries(stats);
  paintHistory(stats);
}

function paintRequests(stats) {
  paintOnce($('#panel-requests'), stats?.requests ?? null, () => {
    if (!stats) return note('Counting…');
    const { movie = {}, episode = {} } = stats.requests ?? {};
    const row = (name, key, tone) => h('tr', {}, h('th', { scope: 'row' }, name),
      h('td', { dataset: { tone: Number(movie[key]) && tone ? tone : '' } }, count(movie[key])),
      h('td', { dataset: { tone: Number(episode[key]) && tone ? tone : '' } }, count(episode[key])));
    return [
      h('table', { class: 'figure-table' },
        h('thead', {}, h('tr', {}, h('td'), h('th', { scope: 'col' }, 'Films'), h('th', { scope: 'col' }, 'Episodes'))),
        h('tbody', {}, row('Fetched by Mynou', 'fetched'), row('Already in Plex', 'in_plex'), row('In progress', 'in_progress'),
          row('Failed', 'failed', 'bad'), row('Cancelled', 'cancelled'))),
      stats.partial ? note('Counted from the newest 200,000 jobs.') : null,
    ];
  });
}

function paintWatchlist(stats) {
  const watchlist = stats?.watchlist;
  paintOnce($('#panel-watchlist'), watchlist ?? stats?.plex_error ?? null, () => {
    if (!stats) return note('Counting…');
    if (!watchlist) return note('Connect Plex in Settings to follow your watchlist here.');
    if (watchlist.error) return note(plexError(watchlist.error), 'bad');
    const films = watchlist.films ?? {};
    const waiting = (Number(films.missing) || 0) + (Number(films.cancelled) || 0);
    return [
      figures([
        ['Films', count(films.total)],
        ['Series', count(watchlist.series?.total)],
      ]),
      figures([
        ['Films already in Plex', count(films.in_plex)],
        ['Films fetched by Mynou', count(films.fetched)],
        ['Films in progress', count(films.in_progress)],
        Number(films.failed) ? ['Films that failed', count(films.failed), 'bad'] : null,
        waiting ? ['Films not requested yet', count(waiting), 'warn'] : null,
        ['Series followed', `${count(watchlist.series?.followed)} of ${count(watchlist.series?.total)}`],
      ]),
      note(`Read ${relativeTime(watchlist.checked_at)}.`),
    ];
  });
}

// Upload speed comes from the service's running total between two pushes.
let uploads = null;
function uploadRate(status) {
  const bytes = Number(status?.uploaded_bytes) || 0;
  const at = Date.now();
  if (!uploads || bytes < uploads.bytes) { uploads = { bytes, at, rate: 0 }; return 0; }
  const seconds = (at - uploads.at) / 1000;
  if (seconds >= 1) {
    uploads = { bytes, at, rate: seconds > 30 ? 0 : (bytes - uploads.bytes) / seconds };
  }
  return uploads.rate;
}

function paintDownloads(status) {
  const el = $('#panel-downloads');
  if (!status) { paintOnce(el, null, () => note('Counting…')); return; }
  const transfers = itemsOf(data.get('transfers')?.data);
  const running = transfers.filter((t) => t.running).length;
  const down = transfers.reduce((sum, t) => sum + rateOf(t), 0);
  const up = uploadRate(status);
  const downloaded = Number(status.downloaded_bytes) || 0;
  const uploaded = Number(status.uploaded_bytes) || 0;
  paintOnce(el, [running, transfers.length, Math.round(down / 1024), Math.round(up / 1024), downloaded, uploaded], () => figures([
    ['Transfers running', `${count(running)} of ${count(transfers.length)}`],
    ['Download speed', `${formatBytes(down)}/s`],
    ['Upload speed', `${formatBytes(up)}/s`],
    ['Downloaded', formatBytes(downloaded)],
    ['Uploaded', formatBytes(uploaded)],
    ['Share ratio', downloaded ? (uploaded / downloaded).toFixed(2) : '–'],
  ]));
}

function paintHealth(stats, status) {
  const el = $('#panel-health');
  if (!status) { paintOnce(el, null, () => note('Checking…')); return; }
  const vpn = status.vpn ?? {};
  const [vpnText, vpnTone] = VPN_STATES[vpn.state] ?? [sentence(vpn.state), 'warn'];
  const plexOn = Boolean(stats?.plex);
  const reports = indexerReports();
  const broken = reports.filter((r) => r.last_error && r.last_error !== 'disabled');
  const enabled = reports.filter((r) => r.enabled !== false);
  const syncedAt = Number(status.last_sync_at) || null;
  const sync = status.last_sync_error ? [plexError(status.last_sync_error), 'bad']
    : syncedAt ? [relativeTime(syncedAt), 'ok']
    : plexOn ? ['Not read yet', 'warn'] : ['Plex not connected', 'warn'];
  const indexers = !enabled.length ? ['None set up', 'warn']
    : broken.length ? [`${broken.length} of ${enabled.length} failing: ${INDEXER_ERRORS[broken[0].last_error] ?? sentence(broken[0].last_error)}`, 'bad']
    : [`${enabled.length} working`, 'ok'];
  const failures = itemsOf(stats?.health?.failures);
  paintOnce(el, [vpnText, sync, indexers, failures.map((f) => f.id), relativeTime(syncedAt)], () => [
    h('ul', { class: 'health' },
      [['VPN', vpnText, vpn.state === 'protected' ? 'ok' : vpnTone], ['Plex watchlist', ...sync], ['Indexers', ...indexers]].map(([name, text, tone]) =>
        h('li', {}, h('span', {}, name), h('span', { class: 'badge', dataset: { tone } }, text)))),
    failures.length
      ? [h('h3', {}, 'Recent failures'), h('ol', { class: 'failures' }, failures.map((f) =>
        h('li', {}, h('strong', {}, mediaLabel(f.request)), h('span', {}, f.error ?? 'Failed'))))]
      : note('No recent failures.'),
  ]);
}

function paintSeries(stats) {
  const root = $('#series-stats');
  const rows = itemsOf(stats?.series);
  if (!stats) { paintOnce(root, null, () => note('Counting…')); return; }
  if (!rows.length) { paintOnce(root, [], () => h('p', { class: 'empty' }, 'No series followed yet. Add a series to your Plex watchlist, or request one.')); return; }
  if (!root.querySelector('table')) {
    root.dataset.signature = '';
    root.replaceChildren(h('table', { class: 'series-table' },
      h('thead', {}, h('tr', {}, ['Series', 'Aired episodes you have', 'In progress', 'Missing', 'Next episode'].map((t) => h('th', { scope: 'col' }, t)))),
      h('tbody')));
  }
  const ordered = rows.toSorted((a, b) => String(a.request?.title).localeCompare(String(b.request?.title)));
  reconcile($('tbody', root), ordered, (row) => row.id, () => h('tr', {},
    h('th', { scope: 'row' }), h('td', {}, h('div', { class: 'have' }, h('span', { class: 'have-bar' }, h('span', { class: 'have-fill' }), h('span', { class: 'have-busy' })), h('span', { class: 'have-text' }))),
    h('td'), h('td'), h('td')), (tr, row) => {
    const aired = row.aired ?? {};
    const total = Number(aired.total) || 0;
    const have = (Number(aired.fetched) || 0) + (Number(aired.in_plex) || 0);
    const busy = Number(aired.in_progress) || 0;
    const missing = Math.max(0, total - have - busy);
    const [title, haveCell, busyCell, missingCell, nextCell] = tr.children;
    title.textContent = row.request?.title ?? '';
    // Only setting a custom property; the stylesheet draws the bar.
    $('.have-bar', haveCell).style.setProperty('--have', total ? have / total : 0);
    $('.have-bar', haveCell).style.setProperty('--busy', total ? busy / total : 0);
    $('.have-text', haveCell).textContent = `${count(have)} of ${count(total)}`;
    busyCell.textContent = count(busy);
    missingCell.textContent = count(missing);
    missingCell.dataset.tone = missing ? 'warn' : '';
    const next = row.next;
    nextCell.textContent = next ? `${episodeCode(next)} · ${next.air_date}` : row.monitored ? 'None announced' : 'Not followed';
  });
}

// Weekly bars, films under episodes, of what Mynou filed itself.
// The SVG namespace comes from the icon template, so no URL lives in this file.
const svg = (tag, attributes = {}) => {
  const el = document.createElementNS($('#icon-template').content.firstElementChild.namespaceURI, tag);
  for (const [key, value] of Object.entries(attributes)) el.setAttribute(key, value);
  return el;
};
const weekLabel = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
let observer = null;
/** The width inside the frame's padding. */
const innerWidth = (el) => {
  const style = getComputedStyle(el);
  return Math.round(el.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight));
};

function paintHistory(stats) {
  const root = $('#history');
  const weeks = itemsOf(stats?.history);
  const width = innerWidth(root);
  if (!observer) {
    observer = new ResizeObserver(() => { if (root.dataset.width !== String(innerWidth(root))) paintHistory(data.get('stats')?.data); });
    observer.observe(root);
  }
  paintOnce(root, [weeks, width], () => {
    root.dataset.width = String(width);
    if (!stats) return note('Counting…');
    const total = weeks.reduce((sum, w) => sum + Number(w.movie) + Number(w.episode), 0);
    if (!total) return h('p', { class: 'empty' }, 'Nothing fetched by Mynou in the last 12 weeks. Films and episodes already in Plex are not counted here.');
    return historyChart(weeks, Math.max(width, 280));
  });
}

function historyChart(weeks, width) {
  const height = 190;
  const pad = { top: 12, right: 8, bottom: 26, left: 32 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;
  const max = Math.max(...weeks.map((w) => Number(w.movie) + Number(w.episode)));
  const step = Math.max(1, Math.ceil(max / 3));
  const top = step * Math.ceil(max / step);
  const y = (n) => pad.top + plotH - (n / top) * plotH;
  const slot = plotW / weeks.length;
  const barW = Math.min(36, slot * 0.6);
  const chart = svg('svg', { viewBox: `0 0 ${width} ${height}`, width, height, role: 'img', 'aria-label': 'Films and episodes fetched by Mynou each week' });
  for (let n = 0; n <= top; n += step) {
    chart.append(svg('line', { class: 'grid', x1: pad.left, x2: width - pad.right, y1: y(n), y2: y(n) }));
    const tick = svg('text', { class: 'tick', x: pad.left - 6, y: y(n) + 4, 'text-anchor': 'end' });
    tick.textContent = String(n);
    chart.append(tick);
  }
  const tip = h('div', { class: 'chart-tip', hidden: true });
  weeks.forEach((week, i) => {
    const films = Number(week.movie) || 0;
    const episodes = Number(week.episode) || 0;
    const x = pad.left + slot * i + (slot - barW) / 2;
    const group = svg('g', { class: 'week' });
    // Films sit on the baseline; episodes stack above with a 2px gap. The top
    // segment alone gets the rounded end.
    const segments = [[films, 'films'], [episodes, 'episodes']].filter(([n]) => n > 0);
    let base = 0;
    segments.forEach(([n, kind], j) => {
      const y0 = y(base) - (j > 0 ? 2 : 0);
      const y1 = y(base + n);
      const last = j === segments.length - 1;
      const r = last ? Math.min(4, barW / 2, (y0 - y1) / 2) : 0;
      const d = `M${x},${y0} V${y1 + r} Q${x},${y1} ${x + r},${y1} H${x + barW - r} Q${x + barW},${y1} ${x + barW},${y1 + r} V${y0} Z`;
      group.append(svg('path', { class: `bar ${kind}`, d }));
      base += n;
    });
    // A hit area taller than the bar, for the tooltip.
    group.append(svg('rect', { class: 'hit', x: pad.left + slot * i, y: pad.top, width: slot, height: plotH }));
    group.addEventListener('pointerenter', () => {
      tip.replaceChildren(h('strong', {}, `Week of ${weekLabel.format(Date.parse(week.week))}`),
        h('span', { class: 'key films' }, `${films} ${films === 1 ? 'film' : 'films'}`),
        h('span', { class: 'key episodes' }, `${episodes} ${episodes === 1 ? 'episode' : 'episodes'}`));
      tip.hidden = false;
      tip.style.left = `${Math.min(width - 150, Math.max(0, x + barW / 2 - 75))}px`;
    });
    group.addEventListener('pointerleave', () => { tip.hidden = true; });
    chart.append(group);
    if (i % 2 === weeks.length % 2 || weeks.length < 7) {
      const label = svg('text', { class: 'tick', x: x + barW / 2, y: height - 8, 'text-anchor': 'middle' });
      label.textContent = weekLabel.format(Date.parse(week.week));
      chart.append(label);
    }
  });
  chart.append(svg('line', { class: 'axis', x1: pad.left, x2: width - pad.right, y1: y(0), y2: y(0) }));
  const legend = h('div', { class: 'chart-legend' }, h('span', { class: 'key films' }, 'Films'), h('span', { class: 'key episodes' }, 'Episodes'));
  const table = h('table', { class: 'visually-hidden' },
    h('thead', {}, h('tr', {}, h('th', {}, 'Week of'), h('th', {}, 'Films'), h('th', {}, 'Episodes'))),
    h('tbody', {}, weeks.map((w) => h('tr', {}, h('td', {}, w.week), h('td', {}, String(w.movie)), h('td', {}, String(w.episode))))));
  return [legend, h('div', { class: 'chart-plot' }, chart, tip), table];
}
