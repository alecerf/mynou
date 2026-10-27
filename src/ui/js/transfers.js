import { label, formatBytes, duration, mediaLabel, fractionOf, relativeTime } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress } from './motion.js';
import { act, openDetail, toast } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, emptyOrError, transferSizes, rateOf, jobsLoaded, transfersTail, currentParam } from './store.js';

// Transfers: the native torrents, with pause and resume, and torrents added by hand.

/** The base64 of a file, without its data URL prefix. */
const base64Of = (file) => new Promise((resolve, reject) => {
  const reader = new FileReader();
  reader.addEventListener('load', () => resolve(String(reader.result).split(',', 2)[1] ?? ''));
  reader.addEventListener('error', () => reject(new Error('The file could not be read')));
  reader.readAsDataURL(file);
});

/** What became of an added torrent, in a sentence. */
function outcome(result) {
  if (result.matched === 'season') {
    const pack = result.pack ?? {};
    const first = itemsOf(pack.jobs)[0]?.request;
    return `Season pack for ${first?.title ?? 'a followed series'}: ${pack.submitted ?? 0} episode${pack.submitted === 1 ? '' : 's'} on their way`;
  }
  if (result.matched) return `Downloading for ${mediaLabel(result.job?.request)}`;
  return `${result.note ?? 'No match in your library'}: downloading it as a plain transfer`;
}

function openAddTorrent() {
  const source = h('input', { name: 'source', type: 'text', placeholder: 'magnet:?xt=… or a link to a .torrent file', spellcheck: 'false', autocomplete: 'off' });
  const file = h('input', { name: 'torrent', type: 'file', accept: '.torrent,application/x-bittorrent' });
  const submit = h('button', { type: 'submit', class: 'primary' }, icon('down'), 'Add');
  const form = h('form', { class: 'sheet' },
    h('p', { class: 'muted' }, 'Mynou matches the release name with your library: a film, an episode or a season of a followed series takes it as its release. Anything else downloads as a plain transfer.'),
    h('label', { class: 'field' }, 'Magnet link or torrent URL', source),
    h('label', { class: 'field' }, 'Or a .torrent file', file),
    h('div', { class: 'actions' }, submit));
  // One source at a time: choosing one clears the other.
  source.addEventListener('input', () => { if (source.value) file.value = ''; });
  file.addEventListener('change', () => { if (file.files.length) source.value = ''; });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    act(submit, async () => {
      const chosen = file.files[0];
      if (!chosen && !source.value.trim()) throw new Error('Paste a magnet link or choose a .torrent file');
      // A .torrent can outgrow a WebSocket frame: it goes over plain HTTP.
      const result = chosen
        ? await link.http('POST', '/api/torrents', { torrent: await base64Of(chosen) })
        : await link.call('POST', '/api/torrents', { source: source.value.trim() });
      $('#detail').close();
      toast(outcome(result), result.matched ? 'ok' : 'warn');
    });
  });
  openDetail('Add a torrent', form);
  source.focus();
}

/** Confirms a removal; the downloaded files go too only when asked. */
function openRemove(transfer, title, removed) {
  const files = h('input', { type: 'checkbox' });
  const remove = h('button', { type: 'submit', class: 'destructive' }, icon('trash'), 'Remove');
  const form = h('form', { class: 'sheet' },
    h('p', { class: 'muted' }, 'The transfer stops and leaves the list. A film or an episode still waiting on it is cancelled; what is already in your library stays.'),
    h('label', { class: 'field check' }, files, 'Also delete the downloaded files'),
    h('div', { class: 'actions' },
      h('button', { type: 'button', class: 'secondary', onclick: () => $('#detail').close() }, 'Keep it'), remove));
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    act(remove, async () => {
      await link.call('POST', `/api/transfers/${encodeURIComponent(transfer.id)}/remove`, { delete_files: files.checked });
      $('#detail').close();
      removed?.();
    }, files.checked ? 'Removed with its files' : 'Removed; the files stay in the download folder');
  });
  openDetail(`Remove ${title}?`, form);
}

// ------------------------------------------------------------ details --
const TRACKER_STATES = {
  working: ['Working', 'ok'], error: ['Error', 'bad'], announcing: ['Announcing', 'accent'], waiting: ['Not contacted yet', 'warn'],
};

const cell = (text, className) => h('td', className ? { class: className } : {}, text);
function table(headings, rows, empty) {
  if (!rows.length) return h('p', { class: 'muted detail-empty' }, empty);
  return h('div', { class: 'table-scroll' }, h('table', { class: 'detail-table' },
    h('thead', {}, h('tr', {}, headings.map(([text, className]) => h('th', className ? { class: className } : {}, text)))),
    h('tbody', {}, rows)));
}

/** A rate from two readings of a growing byte counter; readings too close keep the last rate. */
function rater() {
  const last = new Map();
  return (key, bytes) => {
    const now = Date.now();
    const value = Number(bytes) || 0;
    const before = last.get(key);
    if (before && value >= before.value && now - before.at < 500) return before.rate;
    const rate = before && value >= before.value ? (value - before.value) / ((now - before.at) / 1000) : 0;
    last.set(key, { value, at: now, rate });
    return rate;
  };
}

/** A transfer has no title of its own: the job that started it names it. */
function titleOf(t, owner = jobsLoaded().find((j) => j.download_id === t.id)) {
  const firstFile = Array.isArray(t.files) && t.files[0]?.path ? String(t.files[0].path).split('/')[0] : '';
  return owner ? mediaLabel(owner.request) : t.name || firstFile || `Transfer ${String(t.id).slice(0, 8)}`;
}

const toneOfTransfer = (t) => (t.failed ? 'bad' : t.ready || t.selected_ready ? 'ok' : t.status === 'downloading' ? 'accent' : 'warn');
const stat = (value, text) => h('div', { class: 'stat' }, h('dt', {}, text), h('dd', {}, value));
/** Facts as a grid, each label above its value; empty values are left out. */
const factGrid = (pairs) => h('dl', { class: 'fact-grid' },
  pairs.filter(([, value]) => value !== null && value !== undefined && value !== '')
    .map(([term, value]) => h('div', {}, h('dt', {}, term), h('dd', {}, String(value)))));
const share = (progress) => (progress === null || progress === undefined ? '–' : `${Math.round(progress * 100)}%`);

// ---------------------------------------------------------------- page --
// One transfer in depth at #transfers/ID: what it is, who it talks to and its
// files. The service pushes it every second while the page is on screen.
const page = { id: '', topic: '', rate: rater(), transfer: null, el: null };

function buildPage() {
  const button = (act, name, text, className = 'secondary') =>
    h('button', { type: 'button', class: className, dataset: { act } }, icon(name), text);
  page.el.append(
    h('a', { class: 'text-link back', href: '#transfers' }, '← Transfers'),
    h('header', { class: 'page-heading' },
      h('div', {}, h('h1', { class: 'transfer-title' }), h('p', { class: 'detail-sub transfer-sub' })),
      h('div', { class: 'title-actions' },
        button('pause', 'pause', 'Pause'), button('resume', 'play', 'Resume'), button('remove', 'trash', 'Remove', 'secondary removes'))),
    progressBar('Transfer progress'),
    h('dl', { class: 'summary-strip transfer-stats' }),
    h('div', { class: 'transfer-sections' },
      h('section', { class: 'detail-section transfer-overview' }),
      h('section', { class: 'detail-section transfer-peers' }),
      h('section', { class: 'detail-section transfer-trackers' }),
      h('section', { class: 'detail-section transfer-files' })));
}

function paintPage(t) {
  page.transfer = t;
  const el = page.el;
  const rate = page.rate;
  const fraction = fractionOf(t.progress);
  const total = Number(t.total_bytes) || transferSizes(t).total;
  const verified = Number(t.verified_bytes) || 0;
  const seeding = itemsOf(t.seeding_peers);
  const down = t.running ? rate('down', t.downloaded_bytes) : 0;
  const up = t.running || t.ready ? rate('up', t.uploaded_bytes) : 0;
  const received = Number(t.downloaded_bytes) || 0;
  const sent = Number(t.uploaded_bytes) || 0;
  const title = titleOf(t);
  $('.transfer-title', el).textContent = title;
  document.title = `${title} · Mynou`;
  $('.transfer-sub', el).replaceChildren(h('span', { class: 'badge', dataset: { tone: toneOfTransfer(t) } }, label('transfer', t.status ?? 'queued')), h('span', {}, t.message ?? ''));
  const removing = t.message === 'Removing';
  $('[data-act="pause"]', el).hidden = Boolean(t.paused) || removing;
  $('[data-act="resume"]', el).hidden = !t.paused || removing;
  $('[data-act="remove"]', el).hidden = removing;
  const bar = $('.progress', el);
  bar.hidden = Boolean(t.ready);
  setProgress(bar, `transfer-page:${t.id}`, fraction);
  const connected = itemsOf(t.peers);
  $('.transfer-stats', el).replaceChildren(
    stat(`${Math.round(fraction * 100)}%`, total ? `Done, of ${formatBytes(total)}` : 'Done'),
    stat(t.running ? `${formatBytes(down)}/s` : '–', 'Download'),
    stat(t.running || t.ready ? `${formatBytes(up)}/s` : '–', 'Upload'),
    t.running ? stat(String(connected.length), `Peers, of ${t.known_peers ?? 0} known`) : stat(t.ready ? String(seeding.length) : '–', 'Seeding to'),
    stat(received ? (sent / received).toFixed(2) : '–', 'Ratio'));
  const sources = t.private ? 'Trackers only: private torrent' : ['Trackers', t.dht && 'DHT', t.pex && 'peer exchange'].filter(Boolean).join(', ');
  const limits = [Number(t.effective_policy?.download_limit_bps) && `↓ ${formatBytes(t.effective_policy.download_limit_bps)}/s`,
    Number(t.effective_policy?.upload_limit_bps) && `↑ ${formatBytes(t.effective_policy.upload_limit_bps)}/s`].filter(Boolean).join(' · ');
  $('.transfer-overview', el).replaceChildren(h('h3', {}, 'Overview'), factGrid([
    ['Downloaded', `${formatBytes(Math.min(verified, total || verified))}${total ? ` of ${formatBytes(total)}` : ''}`],
    ['Time left', down && total > verified ? duration((total - verified) / down) : null],
    ['Received', formatBytes(received)],
    ['Sent', formatBytes(sent)],
    ['Seeding for', t.ready && Number(t.seed_elapsed_secs) ? duration(Number(t.seed_elapsed_secs)) : null],
    ['Pieces', Number(t.piece_count) ? `${t.piece_count} × ${formatBytes(Number(t.piece_bytes))}` : 'Waiting for the metadata'],
    ['Finds peers through', sources],
    ['Speed limits', limits || 'None'],
  ]));
  // Downloading, the peers it takes from; complete, the peers it seeds to.
  if (t.running) {
    $('.transfer-peers', el).replaceChildren(h('h3', {}, 'Peers', h('small', {}, String(connected.length))),
      table([['Address'], ['Has', 'num'], ['Received', 'num'], ['Speed', 'num'], ['State']], connected.map((peer) => {
        const speed = rate(`peer:${peer.address}`, peer.received_bytes);
        return h('tr', {}, cell(peer.address, 'mono'), cell(share(peer.progress), 'num'),
          cell(formatBytes(Number(peer.received_bytes) || 0), 'num'),
          cell(speed ? `${formatBytes(speed)}/s` : '', 'num'),
          cell(peer.downloading ? 'Sending' : peer.connected ? 'Connected' : 'Connecting', peer.downloading ? '' : 'quiet'));
      }), 'No peer connected yet.'));
  } else {
    $('.transfer-peers', el).replaceChildren(h('h3', {}, 'Seeding to', h('small', {}, String(seeding.length))),
      table([['Address'], ['Has', 'num'], ['Sent', 'num'], ['Speed', 'num'], ['State']], seeding.map((peer) => {
        const speed = rate(`seed:${peer.address}`, peer.sent_bytes);
        return h('tr', {}, cell(peer.address, 'mono'), cell(share(peer.progress), 'num'),
          cell(formatBytes(Number(peer.sent_bytes) || 0), 'num'),
          cell(speed ? `${formatBytes(speed)}/s` : '', 'num'),
          cell(peer.uploading ? 'Receiving' : peer.interested ? 'Interested' : 'Connected', peer.uploading ? '' : 'quiet'));
      }), t.ready ? 'No peer is downloading from you right now. Peers connect to you when they need pieces you have.' : 'Peers appear here once the transfer runs.'));
  }
  $('.transfer-trackers', el).replaceChildren(h('h3', {}, 'Trackers'),
    table([['Tracker'], ['State'], ['Seeders', 'num'], ['Leechers', 'num'], ['Last answer']], itemsOf(t.trackers).map((tracker) => {
      const [text, tone] = TRACKER_STATES[tracker.status] ?? [tracker.status, 'warn'];
      const count = (value) => (value === null || value === undefined ? '–' : value);
      return h('tr', {}, cell(tracker.host, 'mono'),
        h('td', {}, h('span', { class: 'badge', dataset: { tone } }, text), tracker.error ? h('div', { class: 'hint bad' }, tracker.error) : null),
        cell(count(tracker.seeders), 'num'), cell(count(tracker.leechers), 'num'),
        cell(relativeTime(tracker.last_at)));
    }), 'This torrent names no tracker: peers come from the DHT and peer exchange.'));
  $('.transfer-files', el).replaceChildren(h('h3', {}, 'Files'),
    table([['File'], ['Size', 'num'], ['State']], itemsOf(t.files).map((file) => h('tr', {},
      cell(file.path, 'path'), cell(formatBytes(Number(file.size_bytes) || 0), 'num'),
      cell(file.selected === false ? 'Skipped' : file.verified ? 'Complete' : label('priority', file.priority ?? 'normal')))),
    'The file list arrives with the metadata.'));
}

/** A transfer that left, or never was: the page says so instead of going blank. */
function paintMissing(message) {
  page.transfer = null;
  page.el.replaceChildren(h('a', { class: 'text-link back', href: '#transfers' }, '← Transfers'),
    h('p', { class: 'muted transfer-missing' }, message));
  page.el.dataset.missing = '';
}

function receive(event) {
  if (event.detail.topic !== page.topic) return;
  if (event.detail.status === 200) {
    if ('missing' in page.el.dataset) { delete page.el.dataset.missing; page.el.replaceChildren(); buildPage(); }
    paintPage(event.detail.data);
  } else {
    paintMissing(event.detail.status === 404 || /unknown/i.test(event.detail.data?.error ?? '')
      ? 'This transfer is no longer here.' : event.detail.data?.error ?? 'This transfer could not be read.');
  }
}

function showPage(id) {
  if (page.id === id) return;
  leavePage();
  page.id = id;
  page.topic = `transfer:${id}`;
  page.rate = rater();
  page.transfer = null;
  page.el = $('#transfer-page');
  delete page.el.dataset.missing;
  page.el.replaceChildren();
  buildPage();
  // What the list already knows paints at once; the push follows within a second.
  const known = [...itemsOf(data.get('transfers')?.data), ...transfersTail.items].find((t) => t.id === id);
  if (known) paintPage(known); else $('.transfer-title', page.el).textContent = 'Transfer';
  link.addEventListener('topic', receive);
  link.watch(page.topic);
}

function leavePage() {
  if (!page.id) return;
  link.unwatch(page.topic);
  link.removeEventListener('topic', receive);
  page.id = '';
  page.topic = '';
}

views.transfers = {
  init() {
    $('#torrent-add').addEventListener('click', openAddTorrent);
    $('#transfer-page').addEventListener('click', (event) => {
      const target = event.target.closest('button[data-act]');
      const t = page.transfer;
      if (!target || !t) return;
      if (target.dataset.act === 'remove') { openRemove(t, titleOf(t), () => { location.hash = '#transfers'; }); return; }
      act(target, () => link.call('POST', `/api/transfers/${encodeURIComponent(t.id)}/${target.dataset.act}`, {}),
        target.dataset.act === 'pause' ? 'Paused' : 'Resumed');
    });
    $('#transfers-more').addEventListener('click', (event) =>
      act(event.currentTarget, async () => {
        if (transfersTail.refreshing) return;
        const after = transfersTail.next ?? data.get('transfers')?.data?.next;
        if (!after) return;
        const generation = transfersTail.generation ?? 0;
        const sequence = data.get('transfers')?.data?.sequence;
        const page = await link.call('GET', `/api/transfers?limit=50&after=${encodeURIComponent(after)}`);
        if (generation !== (transfersTail.generation ?? 0) || sequence !== data.get('transfers')?.data?.sequence || page.sequence !== sequence) return;
        transfersTail.items.push(...itemsOf(page.transfers));
        transfersTail.next = page.next;
        views.transfers.render();
      }));
    $('#transfers').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-act]');
      const row = event.target.closest('.row');
      if (row?._transfer && event.target.closest('.open')) { location.hash = `#transfers/${encodeURIComponent(row._transfer.id)}`; return; }
      if (!button || !row?._transfer) return;
      if (button.dataset.act === 'remove') { openRemove(row._transfer, $('.title', row).textContent); return; }
      act(button, () => link.call('POST', `/api/transfers/${encodeURIComponent(row._transfer.id)}/${button.dataset.act}`, {}),
        button.dataset.act === 'pause' ? 'Paused' : 'Resumed');
    });
  },
  leave() { leavePage(); },
  render() {
    const id = currentParam();
    $('#transfers-list').hidden = Boolean(id);
    $('#transfer-page').hidden = !id;
    if (id) { showPage(id); return; }
    leavePage();
    const list = $('#transfers');
    const entry = data.get('transfers')?.data;
    const next = transfersTail.items.length ? transfersTail.next : entry?.next;
    const button = $('#transfers-more');
    button.hidden = !next;
    button.textContent = entry?.total ? `Load more transfers (${itemsOf(entry).length + transfersTail.items.length} of ${entry.total})` : 'Load more transfers';
    if (emptyOrError('transfers', list, 'No native transfers are active.')) return;
    const first = itemsOf(entry);
    const seen = new Set(first.map((item) => item.id));
    const owners = new Map(jobsLoaded().filter((j) => j.download_id).map((j) => [j.download_id, j]));
    reconcile(list, [...first, ...transfersTail.items.filter((item) => !seen.has(item.id))], (t) => t.id,
      () => h('li', { class: 'row' },
        h('span', { class: 'row-state' }),
        h('button', { type: 'button', class: 'open' }, h('div', { class: 'title' }), h('div', { class: 'sub' })),
        h('div', { class: 'buttons' }, h('span', { class: 'badge' }),
          h('button', { type: 'button', class: 'icon-button', dataset: { act: 'pause' }, 'aria-label': 'Pause', title: 'Pause' }, icon('pause')),
          h('button', { type: 'button', class: 'icon-button', dataset: { act: 'resume' }, 'aria-label': 'Resume', title: 'Resume' }, icon('play')),
          h('button', { type: 'button', class: 'icon-button removes', dataset: { act: 'remove' }, 'aria-label': 'Remove', title: 'Remove' }, icon('trash'))),
        progressBar('Transfer progress')),
      (el, t) => {
        const fraction = fractionOf(t.progress);
        $('.title', el).textContent = titleOf(t, owners.get(t.id));
        const { done, total } = transferSizes(t);
        const rate = t.paused ? 0 : rateOf(t);
        $('.sub', el).textContent = [`${Math.round(fraction * 100)}%`, total ? `${formatBytes(Math.min(done, total))} of ${formatBytes(total)}` : formatBytes(done),
          rate && `${formatBytes(rate)}/s`, rate && total > done && `${duration((total - done) / rate)} left`, t.message]
          .filter(Boolean).join(' · ');
        const tone = toneOfTransfer(t);
        const badge = $('.badge', el);
        badge.textContent = label('transfer', t.status ?? 'queued');
        badge.dataset.tone = tone;
        const state = $('.row-state', el);
        const symbol = t.failed ? 'alert' : t.ready || t.selected_ready ? 'check' : t.paused ? 'pause' : 'down';
        if (state.dataset.state !== symbol) {
          const changed = Boolean(state.dataset.state);
          state.replaceChildren(icon(symbol));
          if (changed) Motion.pop(state);
        }
        Object.assign(state.dataset, { tone, state: symbol });
        const bar = $('.progress', el);
        // A finished transfer needs no bar: its badge says so.
        bar.hidden = Boolean(t.ready);
        setProgress(bar, `transfer:${t.id}`, fraction);
        const removing = t.message === 'Removing';
        $('[data-act="pause"]', el).hidden = Boolean(t.paused) || removing;
        $('[data-act="resume"]', el).hidden = !t.paused || removing;
        $('[data-act="remove"]', el).hidden = removing;
        el._transfer = t;
      });
  },
};
