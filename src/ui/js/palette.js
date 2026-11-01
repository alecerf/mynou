import { $, h, icon } from './dom.js';
import { artFor } from './components.js';
import { collectionEntries, openCollectionEntry } from './library.js';
import { VIEWS, STORAGE_VIEW } from './router.js';
import { setTheme } from './theme.js';

// The search palette: one field to find a title, go to a page, request
// something new or run a common action. ⌘K or Ctrl+K opens it from anywhere,
// and so does "/" when no field has the focus.

const dialog = () => $('#palette');
const input = () => $('#palette-input');
const list = () => $('#palette-list');
let choices = [];
let active = 0;

const fold = (text) => String(text ?? '').normalize('NFD').replace(/\p{M}/gu, '').toLocaleLowerCase();
const matches = (text, query) => !query || fold(text).includes(query);

/** Starts the request page on `text`, as a film or as a series. */
function request(text, kind) {
  const form = $('#request-form');
  form.elements.query.value = '';
  form.elements.year.value = '';
  $(`#request-kind [data-kind="${kind}"]`)?.click();
  form.elements.query.value = text;
  form.requestSubmit();
  location.hash = '#requests';
}

function titles(query) {
  if (!query) return [];
  return collectionEntries()
    .map((entry) => ({ entry, request: (entry.type === 'film' ? entry.item.request : entry.request) ?? {} }))
    .filter(({ request: r }) => matches(r.title, query))
    .slice(0, 6)
    .map(({ entry, request: r }) => {
      const series = entry.type !== 'film';
      return {
        group: 'Your collection', title: r.title ?? 'Untitled',
        sub: [Number(r.year) ? r.year : null, series ? 'Series' : r.kind === 'episode' ? 'Episode' : 'Film'].filter(Boolean).join(' · '),
        art: series ? { ...r, kind: 'series' } : r, run: () => openCollectionEntry(entry),
      };
    });
}

function pages(query) {
  const all = [...VIEWS.map(([id, name]) => [id, name]), ['requests', 'New request'], STORAGE_VIEW];
  return all.filter(([, name]) => matches(name, query))
    .map(([id, name]) => ({ group: 'Go to', title: name, icon: id === 'requests' ? 'requests' : id, run: () => { location.hash = `#${id}`; } }));
}

function actions(query, raw) {
  const found = [
    ['Add a torrent', 'down', () => { location.hash = '#transfers'; $('#torrent-add')?.click(); }],
    ['Check for upgrades', 'sync', () => { location.hash = '#library'; $('#check-upgrades')?.click(); }],
    ['Light theme', 'light', () => setTheme('light'), true],
    ['Dark theme', 'dark', () => setTheme('dark'), true],
    ['Theme of the system', 'auto', () => setTheme('auto'), true],
  ].filter(([name]) => matches(name, query))
    .map(([title, symbol, run, stays]) => ({ group: 'Actions', title, icon: symbol, run, stays }));
  const text = raw.trim();
  const asking = text ? [
    { group: 'Request', title: `Find “${text}” as a film`, icon: 'requests', run: () => request(text, 'movie') },
    { group: 'Request', title: `Find “${text}” as a series`, icon: 'series', run: () => request(text, 'series') },
  ] : [];
  return [...asking, ...found];
}

function choose(index) {
  active = Math.max(0, Math.min(choices.length - 1, index));
  for (const [i, el] of [...list().querySelectorAll('[role="option"]')].entries()) {
    const on = i === active;
    el.setAttribute('aria-selected', String(on));
    if (on) { input().setAttribute('aria-activedescendant', el.id); el.scrollIntoView({ block: 'nearest' }); }
  }
}

function render() {
  const raw = input().value;
  const query = fold(raw.trim());
  choices = [...titles(query), ...pages(query), ...actions(query, raw)];
  let group = '';
  const rows = [];
  choices.forEach((choice, i) => {
    if (choice.group !== group) { group = choice.group; rows.push(h('li', { class: 'palette-group', role: 'presentation' }, group)); }
    const thumb = h('span', { class: 'palette-icon' }, choice.icon ? icon(choice.icon) : icon('library'));
    if (choice.art) {
      thumb.classList.add('palette-art');
      artFor(choice.art).then((url) => {
        if (!url || !thumb.isConnected) return;
        thumb.replaceChildren(h('img', { alt: '', decoding: 'async', src: url }));
      });
    }
    rows.push(h('li', { id: `palette-${i}`, role: 'option', 'aria-selected': 'false', dataset: { index: String(i) } },
      thumb, h('span', { class: 'palette-text' }, h('strong', {}, choice.title), choice.sub ? h('small', {}, choice.sub) : null),
      h('kbd', { class: 'palette-enter', 'aria-hidden': 'true' }, '↵')));
  });
  if (!choices.length) rows.push(h('li', { class: 'palette-empty', role: 'presentation' }, 'Nothing matches.'));
  list().replaceChildren(...rows);
  choose(0);
}

function run(index) {
  const choice = choices[index];
  if (!choice) return;
  dialog().close();
  // A sheet left open would cover the page the choice goes to; a theme can change under it.
  if (!choice.stays) $('#detail').close();
  choice.run();
}

export function openPalette(text = '') {
  const el = dialog();
  if (el.open) { input().select(); return; }
  input().value = text;
  render();
  el.showModal();
  input().focus();
}

const typing = (target) => target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));

export function setupPalette() {
  input().addEventListener('input', render);
  input().addEventListener('keydown', (event) => {
    // Enter and the arrows belong to an input method while it composes a word.
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === 'ArrowDown') { event.preventDefault(); choose(active + 1); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); choose(active - 1); }
    else if (event.key === 'Enter') { event.preventDefault(); run(active); }
  });
  list().addEventListener('pointermove', (event) => {
    const option = event.target.closest('[role="option"]');
    if (option && Number(option.dataset.index) !== active) choose(Number(option.dataset.index));
  });
  list().addEventListener('click', (event) => {
    const option = event.target.closest('[role="option"]');
    if (option) run(Number(option.dataset.index));
  });
  dialog().addEventListener('click', (event) => { if (event.target === dialog()) dialog().close(); });
  for (const button of document.querySelectorAll('[data-palette]')) button.addEventListener('click', () => openPalette());
  addEventListener('keydown', (event) => {
    const shortcut = (event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === 'k';
    const slash = event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey && !typing(event.target);
    if (!shortcut && !slash) return;
    if (slash && document.querySelector('dialog[open]')) return;
    event.preventDefault();
    if (shortcut && dialog().open) dialog().close(); else openPalette();
  });
  // The hint says the shortcut of this keyboard.
  const mac = /Mac|iPhone|iPad/.test(navigator.userAgentData?.platform ?? navigator.platform ?? '');
  for (const hint of document.querySelectorAll('.shortcut-hint')) hint.textContent = mac ? '⌘K' : 'Ctrl K';
}
