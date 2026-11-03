import { $, h, icon } from './dom.js';
import { act, busy, openDetail, toast, skeleton, poster, sheetPoster, sheetSection } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { views, itemsOf, loadedItems, jobsLoaded, libraryTail, seriesTail } from './store.js';
import { createMediaCard, paintCover, coverTone } from './library.js';
import { openReleases, requestSearch } from './releases.js';
import { profilePicker, pickedProfile, remember, savedProfiles, bindTitle } from './quality.js';

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
      const meta = $('#catalog-meta');
      const out = $('#catalog-out');
      // Only the latest search may fill the grid.
      const search = ++searches;
      await act($('button[type="submit"]', form), async () => {
        meta.hidden = false;
        $('#request-idle').hidden = true;
        meta.textContent = 'Searching TMDB…';
        out.replaceChildren(skeleton('cards', 10));
        const results = itemsOf((await link.call('GET', `/api/catalog?${params}`)).results);
        if (search !== searches) return;
        const noun = kind === 'series' ? 'series' : results.length === 1 ? 'film' : 'films';
        meta.textContent = results.length ? `${results.length} ${noun} on TMDB` : 'TMDB has nothing by that name. Check the spelling, or leave out the year.';
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
  $('#request-idle').hidden = false;
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
  // The grid may hold a loading skeleton between two searches: only results are marked.
  const state = $('.media-state', card);
  if (!state) return;
  const status = card._title && known(card._title);
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
  // The quality profile a new title uses; the default one unless another is picked.
  const picker = status ? null : profilePicker(film ? 'movie' : 'series');
  if (film) {
    const add = h('button', { type: 'button', class: 'primary', hidden: Boolean(status) }, icon('requests'), 'Add to the library');
    add.addEventListener('click', () => act(add, async () => {
      const profile = pickedProfile(picker);
      await link.call('POST', '/api/jobs', profile ? { ...request, profile } : request);
      remember('movie', request.tmdb_id, profile);
      add.hidden = true;
      picker?.remove();
      marked(['Requested', 'accent']);
      note.textContent = 'Mynou is looking for it on your indexers. Follow it in Jobs.';
      for (const card of $('#catalog-out').children) markCard(card);
    }, 'Added: Mynou is searching the indexers'));
    const choose = h('button', { type: 'button', class: 'secondary', hidden: status?.[0] === 'In your library' }, icon('search'), 'Choose a release');
    choose.addEventListener('click', async () => {
      // A picked profile ranks the list, and the title keeps it once a release is taken.
      const profile = pickedProfile(picker);
      const saved = profile ? (await savedProfiles()).profiles?.[profile] : null;
      const source = requestSearch(request, saved);
      openReleases(label(title), profile ? { ...source, grab: async (id) => { await bindTitle('movie', request.tmdb_id, profile); return source.grab(id); } } : source, back);
    });
    actions.append(...[picker, add, choose].filter(Boolean));
    note.textContent = status ? '' : 'Mynou picks the best release your profile accepts, or you choose one yourself.';
  } else if (!status) {
    const scope = h('select', { 'aria-label': 'Episodes to get' },
      h('option', { value: 'all' }, 'Every episode'), h('option', { value: 'future' }, 'New episodes only'));
    const follow = h('button', { type: 'button', class: 'primary' }, icon('requests'), 'Follow');
    follow.addEventListener('click', () => act(follow, async () => {
      const profile = pickedProfile(picker);
      await link.call('POST', '/api/series', { request, future_only: scope.value === 'future', ...(profile ? { profile } : {}) });
      remember('series', request.tmdb_id, profile);
      scope.remove(); follow.remove(); picker?.remove();
      marked(['Followed', 'ok']);
      note.textContent = scope.value === 'future' ? 'New episodes will be downloaded as they air.' : 'Mynou is looking for the aired episodes. Episodes are in the Library.';
      for (const card of $('#catalog-out').children) markCard(card);
      toast(`Following ${title.title}`);
    }));
    actions.append(picker, scope, follow);
    note.textContent = 'Mynou downloads each episode once it has aired. Pick a release for one episode from its series in the Library.';
  } else {
    actions.append(h('a', { class: 'secondary', href: '#library', onclick: () => $('#detail').close() }, icon('library'), 'Open the library'));
  }
  openDetail({
    title: title.title, tone: coverTone(title.title), crumb: ['New request', film ? 'Film' : 'Series'], eyebrow: film ? 'Film' : 'Series',
    side: h('div', {}, sheetPoster(title.title, coverTone(title.title), art, Number(title.year) ? String(title.year) : '')),
  },
    h('p', { class: 'detail-sub' }, [Number(title.year) ? title.year : null, title.original_title].filter(Boolean).join(' · '), ' ', state),
    actions, note,
    title.overview ? sheetSection('Synopsis', h('p', { class: 'title-overview' }, title.overview)) : null);
}
