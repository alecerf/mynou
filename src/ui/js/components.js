import { $, h, icon } from './dom.js';
import { Motion } from './motion.js';
import { react } from './cat.js';
import { link } from './link.js';

// Small pieces shared by the views: quality tags, fact lists, toasts, the detail
// dialog, and what the page shows while it waits.

export const quality = (traits) => h('div', { class: 'quality' }, traits.map((t) => h('span', {}, t)));

/** Facts with human labels, skipping empty values: [label, value] pairs. */
export function factList(pairs, className = 'facts') {
  const dl = h('dl', { class: className });
  for (const [term, value] of pairs) {
    if (value === undefined || value === null || value === '' || (Array.isArray(value) && !value.length)) continue;
    dl.append(h('dt', {}, term), h('dd', {}, Array.isArray(value) ? value.join('\n') : String(value)));
  }
  return dl;
}
export const technical = (pairs) => h('details', { class: 'technical' }, h('summary', {}, 'Technical details'), factList(pairs));

export function toast(message, tone = 'ok') {
  const el = h('button', { type: 'button', class: 'toast', dataset: { tone }, title: 'Dismiss' }, icon(tone === 'ok' ? 'check' : 'alert'), h('span', {}, message));
  $('#toasts').append(el);
  Motion.animate(el, [{ opacity: 0, transform: 'translateY(24px) scale(.9)' }, { opacity: 1, transform: 'none' }], { duration: 600, easing: Motion.bouncy() });
  if (tone === 'ok') react('happy'); else { Motion.nudge(el); if (tone === 'bad') react('wince'); }
  let timer = 0;
  // The toast is gone from the page at once; only its picture fades.
  const dismiss = () => {
    clearTimeout(timer);
    if (!el.isConnected) return;
    const leaving = Motion.animate(el, [{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'translateY(10px) scale(.94)', filter: 'blur(4px)' }], { duration: 220, fill: 'forwards' });
    el.inert = true;
    if (leaving) leaving.finished.then(() => el.remove(), () => el.remove()); else el.remove();
  };
  const wait = () => { clearTimeout(timer); timer = setTimeout(dismiss, tone === 'bad' ? 7000 : 3500); };
  // A pointer resting on it holds it there to be read.
  el.addEventListener('pointerenter', () => clearTimeout(timer));
  el.addEventListener('pointerleave', wait);
  el.addEventListener('click', dismiss);
  wait();
}

/** Marks a control as working: a spinner takes the place of its icon, or
    comes before its label, once the work lasts long enough to be seen. */
export function busy(el) {
  el.setAttribute('aria-busy', 'true');
  const use = el.querySelector(':scope > .icon use');
  const previous = use?.getAttribute('href');
  let added = null;
  const timer = setTimeout(() => {
    if (use) use.setAttribute('href', '#i-spinner');
    else { added = icon('spinner'); el.prepend(added); }
    el.classList.add('busy');
  }, 120);
  return () => {
    clearTimeout(timer);
    el.removeAttribute('aria-busy');
    el.classList.remove('busy');
    if (use) use.setAttribute('href', previous);
    added?.remove();
  };
}

/** Placeholders shaped like what is coming: list rows, covers, or lines.
    The wrapper carries the `empty` class so views clear it like a message. */
export function skeleton(shape, count = 4) {
  const label = h('span', { class: 'visually-hidden' }, 'Loading…');
  if (shape === 'rows') {
    return h('li', { class: 'empty loading', 'aria-busy': 'true' }, label,
      Array.from({ length: count }, (_, i) => h('div', { class: 'skeleton-row' },
        h('span', { class: 'bone dot' }), h('span', { class: 'bone line', dataset: { width: String(i % 3) } }))));
  }
  if (shape === 'cards') {
    return h('div', { class: 'empty loading', 'aria-busy': 'true' }, label,
      Array.from({ length: count }, () => h('div', { class: 'skeleton-card' }, h('span', { class: 'bone cover-bone' }), h('span', { class: 'bone line' }), h('span', { class: 'bone line short' }))));
  }
  return h('div', { class: 'empty loading', 'aria-busy': 'true' }, label,
    Array.from({ length: count }, (_, i) => h('span', { class: 'bone line', dataset: { width: String(i % 3) } })));
}

const detailCrumb = h('span', { class: 'crumb' });
/** Opens the detail sheet. `heading` is a title, or an object:
    { title, tone, art, crumb, eyebrow, side }. With `side` (a poster column)
    the sheet opens large, as a title's own page; otherwise it is a small dialog.
    `art` is a poster as a data URL, or a promise of one. */
export function openDetail(heading, ...content) {
  const { title, tone, crumb, eyebrow, side } = typeof heading === 'object' ? heading : { title: heading };
  const dialog = $('#detail');
  $('#detail-title').textContent = title;
  if (tone) dialog.dataset.tone = tone; else delete dialog.dataset.tone;
  dialog.dataset.size = side ? 'large' : 'small';
  const eyebrowEl = $('#detail-eyebrow');
  eyebrowEl.textContent = eyebrow ?? '';
  eyebrowEl.hidden = !eyebrow;
  $('#detail-side').hidden = !side;
  $('#detail-side').replaceChildren(...(side ? [side] : []));
  // A way back, when given, takes the place of the path in the top bar.
  const back = content.find((c) => c?.classList?.contains('back'));
  detailCrumb.replaceChildren(...[crumb ?? []].flat().flatMap((part, i) => (i ? [h('span', { 'aria-hidden': 'true' }, '/'), part] : [part])));
  $('#detail .dialog-top').firstElementChild.replaceWith(back ?? detailCrumb);
  $('#detail-body').replaceChildren(...content.filter((c) => c && c !== back));
  $('#detail-scroll').scrollTop = 0;
  // The sheet itself opens in CSS; a sheet already open turns its page.
  if (!dialog.open) dialog.showModal(); else Motion.swap(dialog);
  $('#detail-title').focus();
}

/** A cover: the poster when there is one, else the title on its tone. */
export function sheetPoster(title, tone, art, label) {
  const cover = h('div', { class: 'cover sheet-cover', dataset: { tone } },
    h('span', { class: 'cover-mark' }, label ?? ''), h('span', { class: 'cover-title' }, title));
  Promise.resolve(art).then((url) => {
    if (!url) return;
    const img = h('img', { class: 'cover-art', alt: '', decoding: 'async', src: url });
    img.addEventListener('load', () => cover.classList.add('has-art'), { once: true });
    cover.prepend(img);
  });
  return cover;
}

/** Tabs over panels: [key, label, content]. Arrows, Home and End move between them. */
export function sheetTabs(tabs, first = tabs[0]?.[0]) {
  const list = h('div', { class: 'sheet-tabs', role: 'tablist', 'aria-label': 'About this title' });
  const panels = h('div', { class: 'sheet-panels' });
  const select = (key, focus) => {
    for (const tab of list.children) {
      const on = tab.dataset.tab === key;
      tab.setAttribute('aria-selected', String(on));
      tab.tabIndex = on ? 0 : -1;
      if (on && focus) tab.focus({ preventScroll: true });
    }
    for (const panel of panels.children) panel.hidden = panel.dataset.tab !== key;
  };
  for (const [key, text, content] of tabs) {
    const id = `detail-tab-${key}`;
    list.append(h('button', { type: 'button', role: 'tab', id, dataset: { tab: key }, 'aria-controls': `${id}-panel`, onclick: () => select(key) }, text));
    panels.append(h('section', { role: 'tabpanel', id: `${id}-panel`, 'aria-labelledby': id, tabindex: '0', dataset: { tab: key } }, content));
  }
  list.addEventListener('keydown', (event) => {
    const tabs = [...list.children];
    const index = tabs.indexOf(event.target);
    const next = { ArrowRight: index + 1, ArrowLeft: index - 1 + tabs.length, Home: 0, End: tabs.length - 1 }[event.key];
    if (index < 0 || next === undefined) return;
    event.preventDefault();
    select(tabs[next % tabs.length].dataset.tab, true);
  });
  select(first);
  const root = h('div', { class: 'sheet-tabbed' }, list, panels);
  /** Shows one tab and brings the tab row into view. */
  root.show = (key) => { select(key, true); list.scrollIntoView({ block: 'nearest' }); };
  return root;
}

/** A titled part of a sheet. */
export const sheetSection = (title, ...body) =>
  h('section', { class: 'sheet-section' }, h('header', {}, h('h3', {}, title)), ...body);

/** Facts in two columns, label above value; empty values are left out. */
export function sheetFacts(pairs, className = '') {
  const shown = pairs.filter(([, value]) => value !== undefined && value !== null && value !== '' && value !== false && !(Array.isArray(value) && !value.length));
  if (!shown.length) return null;
  return h('dl', { class: `sheet-facts ${className}`.trim() }, shown.map(([term, value]) =>
    h('div', {}, h('dt', {}, term), h('dd', {}, Array.isArray(value) ? value.join('\n') : value instanceof Node ? value : String(value)))));
}

/** Puts `text` on the clipboard. Over plain HTTP on the local network the
    browser offers no clipboard API: a selected text is copied instead. */
export async function copyText(text) {
  if (window.isSecureContext && navigator.clipboard) { await navigator.clipboard.writeText(text); return; }
  const area = h('textarea', { value: text, readOnly: true, 'aria-hidden': 'true' });
  Object.assign(area.style, { position: 'fixed', inset: '0 auto auto 0', opacity: '0', pointerEvents: 'none' });
  document.body.append(area);
  area.select();
  try {
    if (!document.execCommand('copy')) throw new Error('Copy refused');
  } finally {
    area.remove();
  }
}

/** Copies a text, such as a file path, when the operator asks for it. */
export function copyButton(text, what = 'Path') {
  return h('button', { type: 'button', class: 'secondary small', onclick: async () => {
    try { await copyText(text); toast(`${what} copied`); } catch { toast('This browser does not allow copying here', 'bad'); }
  } }, icon('copy'), `Copy the ${what.toLowerCase()}`);
}

// Posters come from the service as data URLs: the page never loads anything
// from another origin. A few are read at a time, each once.
const posters = new Map();   // TMDB path -> Promise<data URL | null>
const waiting = [];
let reading = 0;
const DATA_URL = /^data:image\/(jpeg|png);base64,[A-Za-z0-9+/=]+$/;
// A poster that failed to load is forgotten, so a later call asks again.
function fetchPoster(path) {
  if (!path) return Promise.resolve(null);
  if (!posters.has(path)) {
    posters.set(path, new Promise((resolve, reject) => { waiting.push({ path, resolve, reject }); pump(); }));
  }
  return posters.get(path);
}
export const poster = (path) => fetchPoster(path).catch(() => null);
function pump() {
  while (reading < 4 && waiting.length) {
    const { path, resolve, reject } = waiting.shift();
    reading += 1;
    link.call('GET', `/api/catalog/poster?path=${encodeURIComponent(path)}`)
      .then((value) => resolve(DATA_URL.test(value?.data_url ?? '') ? value.data_url : null),
        (error) => { posters.delete(path); reject(error); })
      .finally(() => { reading -= 1; pump(); });
  }
}

// A title in the library holds its TMDB id, not its poster: the catalog is
// asked once per title for that id. Without an id, it is searched by name and
// only an entry with the same title and year is used.
const catalogEntries = new Map();   // key -> Promise<catalog result | null>
const catalogKey = (request = {}) => {
  const series = request.kind === 'episode' || request.kind === 'series';
  const title = String(request.title ?? '').trim();
  return title ? `${series ? 'series' : 'movie'}:${request.tmdb_id ?? `${title}:${request.year ?? ''}`}` : '';
};
// A long library scrolls past many titles: the catalog is asked a few at a time.
const searching = [];
let searches = 0;
function catalogCall(path) {
  return new Promise((resolve, reject) => { searching.push({ path, resolve, reject }); nextSearch(); });
}
function nextSearch() {
  while (searches < 3 && searching.length) {
    const { path, resolve, reject } = searching.shift();
    searches += 1;
    link.call('GET', path).then(resolve, reject).finally(() => { searches -= 1; nextSearch(); });
  }
}
/** The catalog entry of `request`; a failed lookup rejects and is forgotten. */
function lookupEntry(request = {}) {
  const series = request.kind === 'episode' || request.kind === 'series';
  const title = String(request.title ?? '').trim();
  const key = catalogKey(request);
  if (!key) return Promise.resolve(null);
  if (!catalogEntries.has(key)) {
    const id = Number(request.tmdb_id);
    const byId = Number.isSafeInteger(id) && id > 0;
    const params = new URLSearchParams({ kind: series ? 'series' : 'movie' });
    if (byId) params.set('tmdb_id', String(id));
    else {
      params.set('query', title);
      if (!series && Number(request.year)) params.set('year', request.year);
    }
    catalogEntries.set(key, catalogCall(`/api/catalog?${params}`).then((answer) => {
      const results = Array.isArray(answer?.results) ? answer.results : [];
      return results.find((r) => (byId ? Number(r.tmdb_id) === id
        : r.title === title && (series || !Number(request.year) || Number(r.year) === Number(request.year)))) ?? null;
    }, (error) => { catalogEntries.delete(key); throw error; }));
  }
  return catalogEntries.get(key);
}
export const catalogEntry = (request) => lookupEntry(request).catch(() => null);
const lookupArt = (request) => lookupEntry(request).then((entry) => fetchPoster(entry?.poster));
/** The poster of a film or series in the library, when TMDB has one. */
export const artFor = (request) => lookupArt(request).catch(() => null);

// A cover asks for its poster only when it comes near the screen.
const nearScreen = typeof IntersectionObserver === 'function'
  ? new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      nearScreen.unobserve(entry.target);
      paintCoverArt(entry.target);
    }
  }, { rootMargin: '600px 0px' })
  : null;
function paintCoverArt(cover) {
  const key = cover._artKey;
  lookupArt(cover._artRequest).then((url) => {
    if (!url || cover._artKey !== key) return;
    const img = h('img', { class: 'cover-art', alt: '', decoding: 'async', src: url });
    img.addEventListener('load', () => { if (cover._artKey === key) cover.classList.add('has-art'); }, { once: true });
    cover.querySelector('.cover-art')?.remove();
    cover.prepend(img);
  }, () => {
    // Offline or refused: the next render after reconnecting asks again.
    if (cover._artKey === key) delete cover._artKey;
  });
}
/** Gives a reused cover the poster of `request`, loaded once the cover is in view. */
export function coverArt(cover, request) {
  const key = catalogKey(request);
  if (cover._artKey === key) return;
  cover._artKey = key;
  cover._artRequest = request;
  cover.classList.remove('has-art');
  cover.querySelector('.cover-art')?.remove();
  if (!key) return;
  if (nearScreen) nearScreen.observe(cover); else paintCoverArt(cover);
}

export async function act(button, work, done) {
  button.disabled = true;
  const idle = busy(button);
  try { await work(); if (done) toast(done); } catch (e) { toast(e.message, 'bad'); } finally { idle(); button.disabled = false; }
}
