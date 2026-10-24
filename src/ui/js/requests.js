import { $, h, icon } from './dom.js';
import { act, busy, openDetail, toast, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { views, itemsOf, loadedItems, jobsLoaded, libraryTail, seriesTail } from './store.js';
import { createMediaCard, paintCover, coverTone } from './library.js';
import { openReleases, requestSearch } from './releases.js';

// New request: find the title on TMDB, then add the film or follow the series.
// Mynou then searches the indexers by itself; a release can also be chosen by hand.

let kind = 'movie';
let searches = 0;

views.requests = {
  init() {
    const form = $('#request-form');
    $('#request-kind').addEventListener('click', (event) => {
      const button = event.target.closest('button');
      if (!button || button.dataset.kind === kind) return;
      kind = button.dataset.kind;
      for (const tab of $('#request-kind').children) tab.setAttribute('aria-pressed', String(tab === button));
      // A title already typed is looked up again in the other catalog.
      if (form.elements.query.value.trim()) form.requestSubmit(); else clear();
    });
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      const params = new URLSearchParams({ kind, query: form.elements.query.value.trim() });
      if (Number(form.elements.year.value)) params.set('year', form.elements.year.value);
      const meta = $('#catalog-meta');
      const out = $('#catalog-out');
      // Only the latest search may fill the grid.
      const search = ++searches;
      await act($('button[type="submit"]', form), async () => {
        meta.hidden = false;
        meta.textContent = 'Searching TMDB…';
        out.replaceChildren(skeleton('cards', 10));
        const results = itemsOf((await link.call('GET', `/api/catalog?${params}`)).results);
        if (search !== searches) return;
        const noun = kind === 'series' ? 'series' : results.length === 1 ? 'film' : 'films';
        meta.textContent = results.length ? `${results.length} ${noun} on TMDB` : 'TMDB has nothing by that name. Check the spelling, or leave the year empty.';
        out.replaceChildren(...results.map((title, index) => titleCard(title, index)));
        Motion.arrive(out.children);
      });
      if (search !== searches) return;
      out.querySelector(':scope > .loading')?.remove();
      if (meta.textContent === 'Searching TMDB…') meta.hidden = true;
    });
  },
  render() {
    // Live updates may change what is already requested or followed.
    for (const card of $('#catalog-out').children) markCard(card);
  },
};

function clear() {
  searches += 1;
  $('#catalog-out').replaceChildren();
  $('#catalog-meta').hidden = true;
}

// Posters come from the service as data URLs: the page never loads anything
// from another origin. A few are read at a time, each once.
const posters = new Map();   // TMDB path -> Promise<data URL | null>
const waiting = [];
let reading = 0;
const DATA_URL = /^data:image\/(jpeg|png);base64,[A-Za-z0-9+/=]+$/;
function poster(path) {
  if (!path) return Promise.resolve(null);
  if (!posters.has(path)) {
    posters.set(path, new Promise((resolve) => { waiting.push({ path, resolve }); pump(); }));
  }
  return posters.get(path);
}
function pump() {
  while (reading < 4 && waiting.length) {
    const { path, resolve } = waiting.shift();
    reading += 1;
    link.call('GET', `/api/catalog/poster?path=${encodeURIComponent(path)}`)
      .then((value) => resolve(DATA_URL.test(value?.data_url ?? '') ? value.data_url : null), () => resolve(null))
      .finally(() => { reading -= 1; pump(); });
  }
}

/** What Mynou already holds for a TMDB title: a film request, or a followed series. */
function known(title) {
  const id = Number(title.tmdb_id);
  if (title.kind === 'series') {
    return loadedItems('series', seriesTail.items).find((s) => Number(s.request?.tmdb_id) === id) ? ['Followed', 'ok'] : null;
  }
  const jobs = [...loadedItems('library', libraryTail.items), ...jobsLoaded()]
    .filter((j) => j.request?.kind === 'movie' && Number(j.request?.tmdb_id) === id && j.state !== 'cancelled');
  if (jobs.some((j) => j.state === 'ready')) return ['In your library', 'ok'];
  return jobs.length ? ['Requested', 'accent'] : null;
}

const label = (title) => (Number(title.year) ? `${title.title} (${title.year})` : title.title);
const requestOf = (title) => {
  const request = { kind: title.kind, title: title.title, tmdb_id: title.tmdb_id };
  if (Number(title.year)) request.year = Number(title.year);
  return request;
};

/** One TMDB result as a cover: its poster when TMDB has one, else a typographic cover. */
function titleCard(title, index) {
  const card = createMediaCard();
  card._title = title;
  card.classList.add('catalog-card');
  paintCover(card, title.title, '', title.year);
  $('h3', card).textContent = title.title;
  $('.year', card).textContent = Number(title.year) ? title.year : '';
  $('.media-info', card).replaceChildren(title.original_title ? h('span', { class: 'original' }, title.original_title) : '');
  const cover = $('.cover', card);
  // The poster may still be on its way: the cover says it heard the click.
  cover.addEventListener('click', () => {
    if (cover.hasAttribute('aria-busy')) return;
    const idle = busy(cover);
    openTitle(title).finally(idle);
  });
  // Stagger the first posters a little so the grid fills in reading order.
  poster(title.poster).then((art) => {
    if (!art) return;
    card.style.setProperty('--delay', `${Math.min(index, 12) * 30}ms`);
    const img = h('img', { class: 'cover-art', alt: '', decoding: 'async', src: art });
    img.addEventListener('load', () => cover.classList.add('has-art'), { once: true });
    cover.prepend(img);
  });
  markCard(card);
  return card;
}

function markCard(card) {
  const status = card._title && known(card._title);
  const state = $('.media-state', card);
  state.hidden = !status;
  if (status) { state.textContent = status[0]; state.dataset.tone = status[1]; }
}

/** The sheet of one title: its poster, synopsis and what can be done with it. */
async function openTitle(title) {
  const art = await poster(title.poster);
  const request = requestOf(title);
  const status = known(title);
  const film = title.kind === 'movie';
  const back = { label: label(title), run: () => openTitle(title) };
  const actions = h('div', { class: 'title-actions' });
  const state = h('span', { class: 'badge', hidden: !status });
  const mark = ([text, tone]) => { state.textContent = text; state.dataset.tone = tone; state.hidden = false; };
  if (status) mark(status);
  const marked = (status) => { mark(status); Motion.pop(state); };
  const note = h('p', { class: 'title-note' });
  if (film) {
    const add = h('button', { type: 'button', class: 'primary', hidden: Boolean(status) }, icon('requests'), 'Add to the library');
    add.addEventListener('click', () => act(add, async () => {
      await link.call('POST', '/api/jobs', request);
      add.hidden = true;
      marked(['Requested', 'accent']);
      note.textContent = 'Mynou is looking for it on your indexers. Follow it in Jobs.';
      for (const card of $('#catalog-out').children) markCard(card);
    }, 'Added: Mynou is searching the indexers'));
    const choose = h('button', { type: 'button', class: 'secondary', hidden: status?.[0] === 'In your library' }, icon('search'), 'Choose a release');
    choose.addEventListener('click', () => openReleases(label(title), requestSearch(request), back));
    actions.append(add, choose);
    note.textContent = status ? '' : 'Mynou picks the best release your profile accepts, or you choose one yourself.';
  } else if (!status) {
    const scope = h('select', { 'aria-label': 'Episodes to get' },
      h('option', { value: 'all' }, 'Every episode'), h('option', { value: 'future' }, 'New episodes only'));
    const follow = h('button', { type: 'button', class: 'primary' }, icon('requests'), 'Follow');
    follow.addEventListener('click', () => act(follow, async () => {
      await link.call('POST', '/api/series', { request, future_only: scope.value === 'future' });
      scope.remove(); follow.remove();
      marked(['Followed', 'ok']);
      note.textContent = scope.value === 'future' ? 'New episodes will be downloaded as they air.' : 'Mynou is looking for the aired episodes. Episodes are in the Library.';
      for (const card of $('#catalog-out').children) markCard(card);
      toast(`Following ${title.title}`);
    }));
    actions.append(scope, follow);
    note.textContent = 'Mynou downloads each episode once it has aired. Pick a release for one episode from its series in the Library.';
  } else {
    actions.append(h('a', { class: 'secondary', href: '#library', onclick: () => $('#detail').close() }, icon('library'), 'Open the library'));
  }
  openDetail({ title: title.title, tone: coverTone(title.title), art },
    h('p', { class: 'detail-sub' }, [Number(title.year) ? title.year : null, film ? 'Film' : 'Series', title.original_title].filter(Boolean).join(' · '), ' ', state),
    title.overview ? h('p', { class: 'title-overview' }, title.overview) : null,
    actions, note);
}
