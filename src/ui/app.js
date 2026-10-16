// Mynou web interface: no framework, no build step, no dependency.
// Untrusted text only ever reaches the DOM through textContent / text nodes.

const TOPICS = ['status', 'jobs', 'transfers', 'library', 'series', 'indexers', 'feeds', 'irc', 'requesters'];
const VIEWS = [
  ['dashboard', 'Dashboard'],
  ['requests', 'Requests'],
  ['jobs', 'Jobs'],
  ['transfers', 'Transfers'],
  ['library', 'Library'],
  ['series', 'Series'],
  ['sources', 'Sources'],
];
const TERMINAL_STATES = new Set(['ready', 'failed', 'cancelled']);
const TOKEN_KEY = 'mynou.token';

const $ = (selector, root = document) => root.querySelector(selector);

/** Tiny DOM builder. Strings become text nodes, never markup. */
function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') el.className = value;
    else if (key === 'dataset') Object.assign(el.dataset, value);
    else if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key in el && key !== 'list') el[key] = value;
    else el.setAttribute(key, value === true ? '' : value);
  }
  el.append(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
  return el;
}

const formatBytes = (value) => {
  let n = Number(value);
  if (!Number.isFinite(n)) return '–';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 10 ? 1 : 0)} ${units[i]}`;
};

const relativeTime = (() => {
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'short' });
  return (iso) => {
    const t = Date.parse(iso);
    if (Number.isNaN(t)) return '';
    const s = Math.round((t - Date.now()) / 1000);
    for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]]) {
      if (Math.abs(s) >= size) return rtf.format(Math.round(s / size), unit);
    }
    return rtf.format(s, 'second');
  };
})();

const toneFor = (state) =>
  state === 'ready' || state === 'imported' ? 'ok' : state === 'failed' || state === 'cancelled' ? 'bad' : 'warn';

/** Updates `container` so it holds one element per item, in order, reusing existing nodes by key. */
function reconcile(container, items, key, create, update) {
  const existing = new Map([...container.children].map((el) => [el.dataset.key, el]));
  let cursor = container.firstElementChild;
  for (const [index, item] of items.entries()) {
    const k = String(key(item, index));
    let el = existing.get(k);
    if (el) existing.delete(k);
    else { el = create(item); el.dataset.key = k; }
    update(el, item);
    if (el !== cursor) container.insertBefore(el, cursor);
    else cursor = cursor.nextElementSibling;
  }
  for (const el of existing.values()) el.remove();
}

function toast(message, tone = 'ok') {
  const el = h('div', { class: 'toast', dataset: { tone } }, message);
  $('#toasts').append(el);
  setTimeout(() => el.remove(), tone === 'bad' ? 7000 : 3500);
}

// ---------------------------------------------------------------- link ----

/** One WebSocket with request/response calls, pushed topics and an HTTP fallback. */
class Link extends EventTarget {
  #socket = null;
  #pending = new Map();
  #next = 1;
  #attempt = 0;
  #timer = 0;
  #token = '';
  state = 'closed';

  get token() { return this.#token; }

  connect(token) {
    this.#token = token;
    this.#open();
  }

  disconnect() {
    clearTimeout(this.#timer);
    this.#token = '';
    this.#socket?.close(1000);
  }

  #setState(state) {
    this.state = state;
    this.dispatchEvent(new CustomEvent('state', { detail: state }));
  }

  #open() {
    clearTimeout(this.#timer);
    if (!this.#token) return;
    this.#setState('connecting');
    const hex = [...new TextEncoder().encode(this.#token)].map((b) => b.toString(16).padStart(2, '0')).join('');
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const socket = new WebSocket(`${scheme}://${location.host}/ws`, ['mynou.v1', `auth.${hex}`]);
    this.#socket = socket;
    let opened = false;
    socket.addEventListener('open', () => {
      opened = true;
      this.#attempt = 0;
      this.#setState('open');
      socket.send(JSON.stringify({ subscribe: TOPICS }));
    });
    socket.addEventListener('message', (event) => this.#receive(event.data));
    socket.addEventListener('close', () => {
      if (this.#socket !== socket) return;
      this.#socket = null;
      for (const { reject } of this.#pending.values()) reject(new Error('Connection lost'));
      this.#pending.clear();
      this.#setState('closed');
      if (!opened && this.#attempt === 0) this.#probe();
      const delay = Math.min(15000, 500 * 2 ** this.#attempt++) * (0.7 + Math.random() * 0.6);
      this.#timer = setTimeout(() => this.#open(), delay);
    });
  }

  /** A failed handshake is a wrong token only if the plain API says so. */
  async #probe() {
    try {
      const response = await fetch('/api/status', { headers: { Authorization: `Bearer ${this.#token}` }, signal: AbortSignal.timeout(5000) });
      if (response.status === 401) this.dispatchEvent(new Event('rejected'));
    } catch { /* unreachable service: keep retrying */ }
  }

  #receive(text) {
    let message;
    try { message = JSON.parse(text); } catch { return; }
    if (message.topic) {
      this.dispatchEvent(new CustomEvent('topic', { detail: message }));
    } else if (this.#pending.has(message.id)) {
      const { resolve, reject } = this.#pending.get(message.id);
      this.#pending.delete(message.id);
      if (message.status >= 200 && message.status < 300) resolve(message.body);
      else reject(new Error(message.body?.error ?? `Request failed (${message.status})`));
    }
  }

  async call(method, path, body) {
    if (this.#socket?.readyState === WebSocket.OPEN) {
      const id = this.#next++;
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => {
          this.#pending.delete(id);
          reject(new Error('The request timed out'));
        }, 60000);
        this.#pending.set(id, {
          resolve: (v) => { clearTimeout(timeout); resolve(v); },
          reject: (e) => { clearTimeout(timeout); reject(e); },
        });
        this.#socket.send(JSON.stringify({ id, method, path, body }));
      });
    }
    const response = await fetch(path, {
      method,
      headers: { Authorization: `Bearer ${this.#token}`, ...(body ? { 'Content-Type': 'application/json' } : {}) },
      body: body ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(60000),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error ?? `Request failed (${response.status})`);
    return data;
  }
}

// --------------------------------------------------------------- state ----

const link = new Link();
const data = new Map();      // topic -> { status, data, at }
const views = {};            // name -> { render(), show() }
let current = '';
let jobStates = new Set();   // active state filters

const itemsOf = (value) => {
  if (Array.isArray(value)) return value;
  if (value && typeof value === 'object') {
    const arrays = Object.values(value).filter(Array.isArray);
    return arrays.flat();
  }
  return [];
};
const scalars = (item) =>
  Object.entries(item ?? {}).filter(([, v]) => ['string', 'number', 'boolean'].includes(typeof v) && v !== '');
const titleOf = (item) =>
  item?.request?.title ?? item?.title ?? item?.name ?? item?.release?.title ?? item?.id ?? '(unnamed)';

function emptyOrError(topic, container, empty) {
  const entry = data.get(topic);
  if (!entry) { container.replaceChildren(h('p', { class: 'empty' }, 'Loading…')); return true; }
  if (entry.status >= 400) {
    container.replaceChildren(h('p', { class: 'empty' }, entry.data?.error ?? `Unavailable (${entry.status})`));
    return true;
  }
  if (!itemsOf(entry.data).length && empty) { container.replaceChildren(h('p', { class: 'empty' }, empty)); return true; }
  return false;
}

function facts(item, skip = []) {
  const dl = h('dl', { class: 'facts' });
  for (const [k, v] of scalars(item)) {
    if (skip.includes(k)) continue;
    dl.append(h('dt', {}, k.replaceAll('_', ' ')), h('dd', {}, String(v)));
  }
  return dl;
}

function openDetail(title, ...content) {
  $('#detail-title').textContent = title;
  $('#detail-body').replaceChildren(...content);
  $('#detail').showModal();
}

async function act(button, work, done) {
  button.disabled = true;
  try { await work(); if (done) toast(done); } catch (e) { toast(e.message, 'bad'); } finally { button.disabled = false; }
}

// --------------------------------------------------------------- views ----

views.dashboard = {
  init() { wireJobRows($('#recent')); },
  render() {
    const status = data.get('status')?.data;
    const stats = $('#stats');
    if (!status) { stats.replaceChildren(h('p', { class: 'empty' }, 'Loading…')); return; }
    const cards = [
      ['Active jobs', status.active], ['Ready', status.ready], ['Total jobs', status.jobs],
      ['Transfers', status.torrents], ['Monitored series', status.monitored_series], ['Version', status.version],
    ];
    reconcile(stats, cards, (c) => c[0],
      () => h('div', { class: 'stat' }, h('b'), h('span')),
      (el, [label, value]) => { $('b', el).textContent = value ?? '–'; $('span', el).textContent = label; });
    const alerts = [status.maintenance_error && `Maintenance: ${status.maintenance_error}`,
      status.last_sync_error && `Last sync: ${status.last_sync_error}`, status.stopped && 'The service is stopping.'].filter(Boolean);
    $('#alerts').replaceChildren(...alerts.map((a) => h('p', { class: 'alert', role: 'alert' }, a)));
    const jobs = itemsOf(data.get('jobs')?.data).toSorted((a, b) => String(b.updated_at).localeCompare(String(a.updated_at))).slice(0, 8);
    const recent = $('#recent');
    if (!jobs.length) recent.replaceChildren(h('li', { class: 'empty' }, 'No jobs yet. Make a request to get started.'));
    else reconcile(recent, jobs, (j) => j.id, createJobRow, updateJobRow);
  },
};

function createJobRow() {
  return h('li', { class: 'row' },
    h('button', { class: 'open', type: 'button' }, h('span', { class: 'title' }), h('span', { class: 'sub' })),
    h('div', { class: 'buttons' }, h('span', { class: 'badge' }), h('button', { type: 'button', dataset: { act: 'retry' } }, 'Retry'),
      h('button', { type: 'button', class: 'danger', dataset: { act: 'cancel' } }, 'Cancel')));
}

function updateJobRow(el, job) {
  const request = job.request ?? {};
  const label = request.kind === 'episode'
    ? `${request.title} S${String(request.season).padStart(2, '0')}E${String(request.episode).padStart(2, '0')}`
    : `${request.title ?? titleOf(job)}${request.year ? ` (${request.year})` : ''}`;
  $('.title', el).textContent = label;
  $('.sub', el).textContent = job.last_error ? `${job.state} · ${job.last_error}` : `${request.kind ?? 'job'} · updated ${relativeTime(job.updated_at)}`;
  const badge = $('.badge', el);
  badge.textContent = job.state;
  badge.dataset.tone = toneFor(job.state);
  $('[data-act="retry"]', el).hidden = !(job.state === 'failed' || job.state === 'cancelled');
  $('[data-act="cancel"]', el).hidden = TERMINAL_STATES.has(job.state);
  el._job = job;
}

function wireJobRows(list) {
  list.addEventListener('click', async (event) => {
    const row = event.target.closest('.row');
    if (!row?._job) return;
    const button = event.target.closest('button');
    const job = row._job;
    if (button?.dataset.act) {
      await act(button, () => link.call('POST', `/api/jobs/${encodeURIComponent(job.id)}/${button.dataset.act}`, {}),
        button.dataset.act === 'retry' ? 'Retry queued' : 'Cancelled');
    } else if (button) {
      openJob(job);
    }
  });
}

async function openJob(job) {
  const events = h('ol', { class: 'list' }, h('li', { class: 'muted' }, 'Loading events…'));
  openDetail(titleOf(job), facts({ ...job, ...job.request }, ['updated_at']), h('h2', {}, 'Events'), events);
  try {
    const list = itemsOf(await link.call('GET', `/api/jobs/${encodeURIComponent(job.id)}/events`));
    events.replaceChildren(...(list.length ? list : [{ message: 'No events recorded.' }]).map((e) =>
      h('li', { class: 'sub' }, [e.at, e.state, e.message].filter(Boolean).join(' · '))));
  } catch (e) { events.replaceChildren(h('li', { class: 'error' }, e.message)); }
}

// The live topic carries the newest page; older pages are read on demand, so
// the list is never limited by how many jobs exist.
const jobsTail = { items: [], next: null };
const jobsLoaded = () => {
  const first = itemsOf(data.get('jobs')?.data);
  const seen = new Set(first.map((j) => j.id));
  return [...first, ...jobsTail.items.filter((j) => !seen.has(j.id))];
};
// The filter and state run on the server, so they find jobs on pages that are
// not loaded. Each request examines a bounded window of jobs; a search reads a
// few windows at once and "Search further" continues from its cursor.
const JOB_STATES = ['queued', 'processing', 'downloading', 'importing', 'ready', 'failed', 'cancelled'];
const SEARCH_WINDOWS = 4;
const jobSearch = { query: '', items: [], next: null, scanned: 0, total: null, generation: 0, busy: false };
function jobQuery() {
  const params = new URLSearchParams();
  const needle = $('#job-filter').value.trim();
  if (needle) params.set('q', needle);
  const [state] = jobStates;
  if (state) params.set('state', state);
  return params.toString();
}
async function searchJobs(more) {
  const query = jobQuery();
  if (!more) {
    jobSearch.generation += 1;
    Object.assign(jobSearch, { query, items: [], next: null, scanned: 0, total: null, busy: false });
    if (!query) { views.jobs.render(); return; }
  }
  if (jobSearch.busy || (more && !jobSearch.next)) return;
  const generation = jobSearch.generation;
  jobSearch.busy = true;
  views.jobs.render();
  try {
    let windows = 0;
    do {
      const after = jobSearch.next ? `&after=${encodeURIComponent(jobSearch.next)}` : '';
      const page = await link.call('GET', `/api/jobs?limit=50&${query}${after}`);
      if (generation !== jobSearch.generation) return;
      jobSearch.items.push(...itemsOf(page.jobs));
      jobSearch.next = page.next;
      jobSearch.scanned += Number(page.scanned) || 0;
      jobSearch.total = page.total;
      windows += 1;
    } while (jobSearch.next && jobSearch.items.length < 50 && windows < SEARCH_WINDOWS);
  } catch (error) {
    if (generation === jobSearch.generation) toast(error.message, 'bad');
  } finally {
    if (generation === jobSearch.generation) { jobSearch.busy = false; views.jobs.render(); }
  }
}
views.jobs = {
  init() {
    wireJobRows($('#jobs'));
    $('#jobs-more').addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        if (jobSearch.query) { await searchJobs(true); return; }
        if (jobsTail.refreshing) return;
        const after = jobsTail.next ?? data.get('jobs')?.data?.next;
        if (!after) return;
        const generation = jobsTail.generation ?? 0;
        const sequence = data.get('jobs')?.data?.sequence;
        const page = await link.call('GET', `/api/jobs?limit=50&after=${encodeURIComponent(after)}`);
        if (generation !== (jobsTail.generation ?? 0) || sequence !== data.get('jobs')?.data?.sequence || page.sequence !== sequence) return;
        jobsTail.items.push(...itemsOf(page.jobs));
        jobsTail.next = page.next;
        views.jobs.render();
      }));
    let typing = 0;
    $('#job-filter').addEventListener('input', () => { clearTimeout(typing); typing = setTimeout(() => searchJobs(false), 250); });
    $('#job-states').addEventListener('click', (event) => {
      const button = event.target.closest('button');
      if (!button) return;
      const state = button.dataset.state;
      const active = jobStates.has(state);
      jobStates = new Set(active ? [] : [state]);
      searchJobs(false);
    });
  },
  render() {
    const list = $('#jobs');
    const entry = data.get('jobs')?.data;
    const more = $('#jobs-more');
    reconcile($('#job-states'), JOB_STATES, (s) => s,
      (s) => h('button', { type: 'button', dataset: { state: s }, 'aria-pressed': 'false' }, s),
      (el, s) => el.setAttribute('aria-pressed', String(jobStates.has(s))));
    if (jobSearch.query) {
      // Live rows replace their stale search copies; membership stays the search's.
      const live = new Map(jobsLoaded().map((j) => [j.id, j]));
      const shown = jobSearch.items.map((j) => live.get(j.id) ?? j);
      more.hidden = !jobSearch.next || jobSearch.busy;
      more.textContent = `Search further (${jobSearch.scanned} of ${jobSearch.total ?? '?'} jobs searched)`;
      if (!shown.length) {
        list.replaceChildren(h('li', { class: 'empty' }, jobSearch.busy ? 'Searching…' : jobSearch.next ? 'No match yet; search further.' : 'No job matches the filter.'));
      } else { list.querySelector('.empty')?.remove(); reconcile(list, shown, (j) => j.id, createJobRow, updateJobRow); }
      return;
    }
    const all = jobsLoaded();
    const next = jobsTail.items.length ? jobsTail.next : entry?.next;
    more.hidden = !next;
    more.textContent = entry?.total ? `Load more jobs (${all.length} of ${entry.total})` : 'Load more jobs';
    if (emptyOrError('jobs', list, 'No jobs yet.')) return;
    const shown = all.toSorted((a, b) => String(b.updated_at).localeCompare(String(a.updated_at)));
    list.querySelector('.empty')?.remove();
    reconcile(list, shown, (j) => j.id, createJobRow, updateJobRow);
  },
};

const transfersTail = { items: [], next: null };
views.transfers = {
  init() {
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
      if (!button || !row?._transfer) return;
      act(button, () => link.call('POST', `/api/transfers/${encodeURIComponent(row._transfer.id)}/${button.dataset.act}`, {}),
        button.dataset.act === 'pause' ? 'Paused' : 'Resumed');
    });
  },
  render() {
    const list = $('#transfers');
    const entry = data.get('transfers')?.data;
    const next = transfersTail.items.length ? transfersTail.next : entry?.next;
    const button = $('#transfers-more');
    button.hidden = !next;
    button.textContent = entry?.total ? `Load more transfers (${itemsOf(entry).length + transfersTail.items.length} of ${entry.total})` : 'Load more transfers';
    if (emptyOrError('transfers', list, 'No native transfers are active.')) return;
    const first = itemsOf(entry);
    const seen = new Set(first.map((item) => item.id));
    reconcile(list, [...first, ...transfersTail.items.filter((item) => !seen.has(item.id))], (t) => t.id,
      () => h('li', { class: 'row' },
        h('div', {}, h('div', { class: 'title' }), h('div', { class: 'sub' })),
        h('div', { class: 'buttons' }, h('span', { class: 'badge' }), h('button', { type: 'button', dataset: { act: 'pause' } }, 'Pause'),
          h('button', { type: 'button', dataset: { act: 'resume' } }, 'Resume')),
        h('div', { class: 'bar-track', role: 'progressbar', 'aria-valuemin': 0, 'aria-valuemax': 100 }, h('div', { class: 'bar-fill' }))),
      (el, t) => {
        const fraction = Math.min(1, Math.max(0, Number(t.progress) > 1 ? Number(t.progress) / 100 : Number(t.progress) || 0));
        $('.title', el).textContent = t.name ?? t.title ?? t.id;
        $('.sub', el).textContent = `${Math.round(fraction * 100)}% · ${formatBytes(t.verified_bytes)}${t.message ? ` · ${t.message}` : ''}`;
        const badge = $('.badge', el);
        badge.textContent = t.status ?? 'unknown';
        badge.dataset.tone = t.failed ? 'bad' : t.ready ? 'ok' : 'warn';
        $('.bar-fill', el).style.setProperty('--p', fraction);
        $('.bar-track', el).setAttribute('aria-valuenow', Math.round(fraction * 100));
        $('[data-act="pause"]', el).hidden = Boolean(t.paused);
        $('[data-act="resume"]', el).hidden = !t.paused;
        el._transfer = t;
      });
  },
};

function cards(topic, container, empty, render, more = []) {
  if (emptyOrError(topic, container, empty)) return;
  const first = itemsOf(data.get(topic).data);
  const seen = new Set(first.map((item) => item.id));
  reconcile(container, [...first, ...more.filter((item) => !seen.has(item.id))], (item, i) => item.id ?? `${titleOf(item)}:${i}`,
    () => h('article', { class: 'card' }, h('h3'), h('div', { class: 'body' })),
    (el, item) => { $('h3', el).textContent = titleOf(item); $('.body', el).replaceChildren(render(item)); });
}

const libraryTail = { items: [], next: null };
// Refresh loaded pages in place when jobs outside the first page change.
// A progress update keeps the user's loaded window; membership changes reset it.
function resetTail(tail) {
  tail.items = []; tail.next = null;
  tail.generation = (tail.generation ?? 0) + 1;
}
async function refreshTail(topic, tail, first) {
  if (tail.refreshing || !tail.items.length) return;
  tail.refreshing = true;
  const generation = tail.generation ?? 0;
  const wanted = tail.items.length;
  const rows = [];
  let after = first.next;
  try {
    while (after && rows.length < wanted) {
      const page = await link.call('GET', `/api/${topic}?limit=50&after=${encodeURIComponent(after)}`);
      if (page.sequence !== first.sequence) return;
      rows.push(...itemsOf(page[topic]));
      after = page.next;
    }
    if (generation === (tail.generation ?? 0) && data.get(topic)?.data?.sequence === first.sequence) {
      tail.items = rows; tail.next = after;
      if (current === topic) views[topic].render();
    }
  } catch (error) {
    toast(error.message, 'bad');
  } finally {
    tail.refreshing = false;
    const latest = data.get(topic)?.data;
    if (latest && tail.items.length && (latest.sequence !== first.sequence ||
      (topic === 'transfers' && latest.revision !== first.revision))) refreshTail(topic, tail, latest);
  }
}
function pageMembership(page, topic) {
  return JSON.stringify([itemsOf(page?.[topic]).map((row) => row.id), page?.next]);
}
views.library = {
  init() {
    $('#library-more').addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        if (libraryTail.refreshing) return;
        const after = libraryTail.next ?? data.get('library')?.data?.next;
        if (!after) return;
        const generation = libraryTail.generation ?? 0;
        const sequence = data.get('library')?.data?.sequence;
        const page = await link.call('GET', `/api/library?limit=50&after=${encodeURIComponent(after)}`);
        if (generation !== (libraryTail.generation ?? 0) || sequence !== data.get('library')?.data?.sequence || page.sequence !== sequence) return;
        libraryTail.items.push(...itemsOf(page.library));
        libraryTail.next = page.next;
        views.library.render();
      }));
    $('#check-upgrades').addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        const report = await link.call('POST', '/api/upgrades', {});
        openDetail('Upgrade check', facts(report));
      }));
  },
  render() {
    cards('library', $('#library'), 'Nothing in the library yet.', (item) => facts(item, ['id']), libraryTail.items);
    const entry = data.get('library')?.data;
    const next = libraryTail.items.length ? libraryTail.next : entry?.next;
    const button = $('#library-more');
    button.hidden = !next;
    button.textContent = entry?.total ? `Load more library entries (${itemsOf(entry).length + libraryTail.items.length} of ${entry.total})` : 'Load more library entries';
  },
};
// The live topic carries the first page; further pages are read on demand, so
// the list is never limited by how many series are tracked.
const seriesTail = { items: [], next: null };
views.series = {
  init() {
    $('#series-more').addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        const after = seriesTail.next ?? data.get('series')?.data?.next;
        if (!after) return;
        const page = await link.call('GET', `/api/series?limit=50&after=${encodeURIComponent(after)}`);
        seriesTail.items.push(...itemsOf(page.series));
        seriesTail.next = page.next;
        views.series.render();
      }));
  },
  render() {
    cards('series', $('#series'), 'No series are tracked.', (item) => facts(item, ['id']), seriesTail.items);
    const entry = data.get('series')?.data;
    const next = seriesTail.items.length ? seriesTail.next : entry?.next;
    const button = $('#series-more');
    button.hidden = !next;
    button.textContent = entry?.total ? `Load more series (${itemsOf(entry).length + seriesTail.items.length} of ${entry.total})` : 'Load more series';
  },
};

views.sources = {
  init() {
    $('#sync-now').addEventListener('click', (e) =>
      act(e.currentTarget, async () => { const r = await link.call('POST', '/api/sync', {}); toast(`Submitted ${r.submitted ?? 0} request(s)`); }));
  },
  render() {
    const root = $('#sources');
    const groups = [['indexers', 'Indexer'], ['feeds', 'Feed'], ['irc', 'IRC'], ['requesters', 'Account']];
    const items = groups.flatMap(([topic, label]) => itemsOf(data.get(topic)?.data).map((item) => ({ ...item, label, _key: `${topic}:${item.id ?? titleOf(item)}` })));
    if (!items.length) { root.replaceChildren(h('p', { class: 'empty' }, data.has('indexers') ? 'No sources are configured.' : 'Loading…')); return; }
    root.querySelector('.empty')?.remove();
    reconcile(root, items, (item, i) => `${item._key}:${i}`,
      () => h('article', { class: 'card' }, h('h3'), h('div', { class: 'body' })),
      (el, item) => { $('h3', el).textContent = `${item.label} · ${titleOf(item)}`; $('.body', el).replaceChildren(facts(item, ['id', 'label', '_key'])); });
  },
};

views.requests = {
  init() {
    const form = $('#request-form');
    const sync = () => { form.dataset.kind = form.elements.kind.value; };
    form.elements.kind.addEventListener('change', sync);
    sync();
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      const request = this.request(form);
      const button = $('button[type="submit"]', form);
      await act(button, async () => {
        $('#search-out').replaceChildren(h('p', { class: 'empty' }, 'Searching…'));
        this.show(request, await link.call('POST', '/api/search', request));
      }).catch(() => {});
      if ($('#search-out').textContent === 'Searching…') $('#search-out').replaceChildren();
    });
  },
  request(form) {
    const field = (name) => form.elements[name].value;
    const number = (name) => (field(name) ? Number(field(name)) : undefined);
    const request = { kind: field('kind'), title: field('title').trim() };
    if (number('year')) request.year = number('year');
    if (request.kind === 'episode') { request.season = number('season') ?? 0; request.episode = number('episode') ?? 0; }
    return request;
  },
  // The first page arrives with the search; further candidate pages repeat the
  // live search after the last cursor, so the list has no fixed length.
  show(request, report) {
    const accepted = itemsOf(report.accepted);
    const rejected = itemsOf(report.rejected);
    const out = $('#search-out');
    const submit = h('button', { type: 'button', class: 'primary', disabled: !report.selected_candidate_id }, 'Submit request');
    submit.addEventListener('click', () => act(submit, async () => {
      await link.call('POST', '/api/jobs', request);
      out.replaceChildren(h('p', { class: 'empty' }, 'Request submitted.'));
      location.hash = '#jobs';
    }, 'Request submitted'));
    const total = Number(report.candidate_count) || 0;
    const summary = h('span', { class: 'muted' });
    const acceptedList = h('ol', { class: 'list' }, accepted.map((c) => candidateRow(c)),
      report.selected_candidate_id ? null : h('li', { class: 'empty' }, 'No acceptable release was found. Nothing will be submitted unless you try again.'));
    const rejectedList = h('ol', { class: 'list' }, rejected.map((c) => candidateRow(c)));
    const rejectedSummary = h('summary');
    const more = h('button', { type: 'button' });
    let next = report.next;
    const update = () => {
      summary.textContent = `${accepted.length} acceptable · ${rejected.length} rejected · ${accepted.length + rejected.length} of ${total} candidates`;
      rejectedSummary.textContent = `${rejected.length} rejected`;
      more.hidden = !next;
      more.textContent = 'Load more candidates';
    };
    more.addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        if (!next) return;
        const page = await link.call('POST', `/api/search?limit=100&after=${encodeURIComponent(next)}`, request);
        const moreAccepted = itemsOf(page.accepted);
        const moreRejected = itemsOf(page.rejected);
        accepted.push(...moreAccepted);
        rejected.push(...moreRejected);
        acceptedList.append(...moreAccepted.map((c) => candidateRow(c)));
        rejectedList.append(...moreRejected.map((c) => candidateRow(c)));
        next = page.next;
        update();
      }));
    update();
    out.replaceChildren(
      h('div', { class: 'actions' }, submit, summary),
      acceptedList,
      h('details', {}, rejectedSummary, rejectedList),
      h('div', { class: 'actions' }, more));
  },
};

/** One search candidate: title, source, seeders, score and the reasons behind the verdict. */
function candidateRow(c) {
  const assessment = c.assessment ?? {};
  const rank = assessment.rank ?? {};
  const traits = [rank.resolution, rank.source, rank.codec, rank.language].filter((v) => typeof v === 'string' && v);
  const reasons = Array.isArray(assessment.reasons) ? assessment.reasons.filter((r) => typeof r === 'string') : [];
  return h('li', { class: 'row' },
    h('div', {},
      h('div', { class: 'title' }, c.title ?? '(untitled)'),
      h('div', { class: 'sub' }, [c.source, c.seeders != null && `${c.seeders} seeders`, ...traits].filter(Boolean).join(' · ')),
      reasons.length ? h('div', { class: 'sub' }, reasons.join(' · ')) : null),
    typeof rank.custom_score === 'number' ? h('span', { class: 'score', title: 'Score' }, String(rank.custom_score)) : null);
}

// ------------------------------------------------------------- routing ----

function show(name) {
  if (!views[name]) name = 'dashboard';
  const apply = () => {
    for (const section of document.querySelectorAll('.view')) section.hidden = section.id !== `view-${name}`;
    for (const button of document.querySelectorAll('#nav button')) {
      if (button.dataset.view === name) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
    }
    current = name;
    document.title = `${VIEWS.find(([id]) => id === name)[1]} · Mynou`;
    views[name].render?.();
  };
  if (document.startViewTransition && !matchMedia('(prefers-reduced-motion: reduce)').matches && current) document.startViewTransition(apply);
  else apply();
}

const route = () => show(location.hash.slice(1));

// ----------------------------------------------------- command palette ----

const commands = [
  ...VIEWS.map(([id, label], i) => ({ label: `Go to ${label}`, key: String(i + 1), run: () => { location.hash = `#${id}`; } })),
  { label: 'Sync now', run: () => link.call('POST', '/api/sync', {}).then((r) => toast(`Submitted ${r.submitted ?? 0} request(s)`)).catch((e) => toast(e.message, 'bad')) },
  { label: 'Disconnect and forget the token', run: () => { try { sessionStorage.removeItem(TOKEN_KEY); } catch { /* storage unavailable */ } link.disconnect(); askToken(); } },
];

function setupPalette() {
  const dialog = $('#palette');
  const input = $('#palette-input');
  const list = $('#palette-list');
  let shown = [];
  let index = 0;
  const paint = () => {
    list.replaceChildren(...shown.map((c, i) => h('li', { role: 'option', 'aria-selected': String(i === index), onclick: () => choose(i) },
      h('span', {}, c.label), c.key ? h('kbd', {}, c.key) : null)));
    list.children[index]?.scrollIntoView({ block: 'nearest' });
  };
  const filter = () => {
    const needle = input.value.trim().toLowerCase();
    shown = commands.filter((c) => c.label.toLowerCase().includes(needle));
    index = 0;
    paint();
  };
  const choose = (i) => { const c = shown[i]; dialog.close(); c?.run(); };
  const open = () => { if (dialog.open) return; input.value = ''; filter(); dialog.showModal(); input.focus(); };
  input.addEventListener('input', filter);
  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown') { index = Math.min(shown.length - 1, index + 1); paint(); event.preventDefault(); }
    else if (event.key === 'ArrowUp') { index = Math.max(0, index - 1); paint(); event.preventDefault(); }
    else if (event.key === 'Enter') { choose(index); event.preventDefault(); }
  });
  dialog.addEventListener('click', (event) => { if (event.target === dialog) dialog.close(); });
  $('#open-palette').addEventListener('click', open);
  document.addEventListener('keydown', (event) => {
    const typing = event.target.closest?.('input, select, textarea, dialog[open]');
    if ((event.key === 'k' && (event.metaKey || event.ctrlKey)) || (event.key === '/' && !typing)) { event.preventDefault(); open(); }
    else if (!typing && !event.metaKey && !event.ctrlKey && !event.altKey && /^[1-7]$/.test(event.key)) location.hash = `#${VIEWS[Number(event.key) - 1][0]}`;
  });
}

// ----------------------------------------------------------- bootstrap ----

function askToken(message = '') {
  const dialog = $('#login');
  $('#login-error').textContent = message;
  if (!dialog.open) dialog.showModal();
  $('input', dialog).focus();
}

function setup() {
  $('#nav').replaceChildren(...VIEWS.map(([id, label]) => h('button', { type: 'button', dataset: { view: id }, onclick: () => { location.hash = `#${id}`; } }, label)));
  for (const view of Object.values(views)) view.init?.();
  $('#detail-close').addEventListener('click', () => $('#detail').close());
  $('#detail').addEventListener('click', (event) => { if (event.target.id === 'detail') event.target.close(); });
  $('#login').addEventListener('cancel', (event) => event.preventDefault());
  $('#login-form').addEventListener('submit', (event) => {
    const token = new FormData(event.currentTarget).get('token').trim();
    event.currentTarget.reset();
    try { sessionStorage.setItem(TOKEN_KEY, token); } catch { /* storage unavailable: the token lives for this page only */ }
    link.connect(token);
  });
  setupPalette();

  const pill = $('#link');
  link.addEventListener('state', ({ detail }) => {
    pill.dataset.state = detail;
    pill.textContent = { open: 'Live', connecting: 'Connecting', closed: 'Offline' }[detail];
    if (detail === 'open') {
      resetTail(jobsTail);
      resetTail(libraryTail);
      resetTail(transfersTail);
      $('#login').close();
    }
  });
  link.addEventListener('rejected', () => {
    // A refused handshake before any success usually means a wrong token.
    if (!data.size) { try { sessionStorage.removeItem(TOKEN_KEY); } catch { /* ignore */ } link.disconnect(); askToken('The token was refused. Check it and try again.'); }
  });
  let queued = false;
  link.addEventListener('topic', ({ detail }) => {
    const previous = data.get(detail.topic);
    const tail = detail.topic === 'jobs' ? jobsTail : detail.topic === 'library' ? libraryTail : detail.topic === 'transfers' ? transfersTail : null;
    if (tail && previous && pageMembership(previous.data, detail.topic) !== pageMembership(detail.data, detail.topic)) resetTail(tail);
    data.set(detail.topic, { status: detail.status, data: detail.data, at: Date.now() });
    if (tail && (previous?.data?.sequence !== detail.data?.sequence ||
      (detail.topic === 'transfers' && previous?.data?.revision !== detail.data?.revision))) refreshTail(detail.topic, tail, detail.data);
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; views[current]?.render?.(); });
  });
  addEventListener('hashchange', route);
  route();

  let token = '';
  try { token = sessionStorage.getItem(TOKEN_KEY) ?? ''; } catch { /* ignore */ }
  if (token) link.connect(token); else askToken();
}

setup();
