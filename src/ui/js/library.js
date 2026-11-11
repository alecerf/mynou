import { label, dateTime, toneOf, qualityOf, episodeCode, mediaLabel, yesNo, JOB_ICONS, jobTone, wantedNote } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion } from './motion.js';
import { quality, factList, openDetail, act, skeleton, sheetPoster, sheetTabs, sheetSection, sheetFacts, catalogEntry, artFor, coverArt } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, titleOf, emptyOrError, loadedItems, libraryTail, seriesTail, jobsLoaded } from './store.js';
import { openReleases, episodeSearch } from './releases.js';
import { openJob, libraryState } from './jobs.js';
import { titleQuality } from './quality.js';

// Library: films, followed series and their episodes.

/** A media card: a typographic cover, the title, quality and state. */
export function createMediaCard() {
  return h('article', { class: 'media-card' },
    h('button', { type: 'button', class: 'cover' }, h('span', { class: 'cover-index' }), h('span', { class: 'cover-title' }), h('span', { class: 'cover-mark', 'aria-hidden': 'true' }), h('span', { class: 'cover-open' }, icon('arrow'))),
    h('div', { class: 'media-caption' }, h('h3'), h('span', { class: 'year' })),
    h('div', { class: 'media-info' }, h('div', { class: 'quality' })),
    h('div', { class: 'media-state' }));
}

/** The tone of a cover; its detail sheet opens on the same one. */
export const coverTone = (title) => toneOf(title, 8);

/** A typographic cover: the title on a quiet tone chosen from it, the year above. */
export function paintCover(el, title, index, year) {
  const cover = $('.cover', el);
  cover.dataset.tone = coverTone(title);
  cover.setAttribute('aria-label', `Open ${title}`);
  $('.cover-title', el).textContent = title;
  $('.cover-index', el).textContent = index;
  $('.cover-mark', el).textContent = Number(year) ? String(year) : '';
}

export function updateLibraryCard(el, item) {
  const request = item.request ?? {};
  const title = request.title ?? titleOf(item);
  paintCover(el, title, request.kind === 'episode' ? episodeCode(request) : '', request.year);
  coverArt($('.cover', el), request);
  $('h3', el).textContent = title;
  $('.year', el).textContent = request.kind === 'episode' ? episodeCode(request) : Number(request.year) ? request.year : '';
  $('.quality', el).replaceChildren(...qualityOf(item.release?.title).map((t) => h('span', {}, t)));
  const [text, tone] = libraryState(item);
  const state = $('.media-state', el);
  state.textContent = text;
  state.dataset.tone = tone;
  state.hidden = !text;
  el._item = item;
}

/** A film or an episode of the library opens on its own sheet. */
export const openLibraryEntry = (item, back) => openJob(item, back);

export const libraryFilter = { kind: '', query: '', layout: 'grid' };

views.library = {
  init() {
    $('#library-more').addEventListener('click', (e) =>
      act(e.currentTarget, async () => {
        if (libraryTail.refreshing) return;
        const after = libraryTail.next ?? data.get('library')?.data?.next;
        if (!after) {
          const seriesAfter = seriesTail.next ?? data.get('series')?.data?.next;
          if (!seriesAfter) return;
          const page = await link.call('GET', `/api/series?limit=50&after=${encodeURIComponent(seriesAfter)}`);
          seriesTail.items.push(...itemsOf(page.series));
          seriesTail.next = page.next;
          views.library.render();
          return;
        }
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
        const entries = itemsOf(report.entries);
        const skipped = report.skipped ?? {};
        openDetail('Upgrade check',
          factList([
            ['Checked', report.checked], ['Queued', report.queued],
            ['Not watched', Number(skipped.unmonitored) || null], ['Release not recorded', Number(skipped.baseline_required) || null],
            ['Not supported', Number(skipped.unsupported_kind) || null],
            ['Limit', report.limited ? 'More entries remain for the next check' : null],
          ]),
          entries.length ? h('ol', { class: 'events' }, entries.map((entry) => {
            const owner = loadedItems('library', libraryTail.items).find((item) => item.id === entry.job_id);
            return h('li', {}, h('span', {}, owner ? mediaLabel(owner.request) : String(entry.job_id ?? '').slice(0, 8)),
              h('span', {}, [label('upgrade', entry.action), entry.candidate_title].filter(Boolean).join(' · ')));
          })) : null);
      }));
    $('#library-tabs').addEventListener('click', (event) => {
      const button = event.target.closest('button');
      if (!button) return;
      libraryFilter.kind = button.dataset.kind;
      views.library.render();
    });
    let typing = 0;
    $('#library-filter').addEventListener('input', (event) => {
      clearTimeout(typing);
      typing = setTimeout(() => { libraryFilter.query = event.target.value.trim().toLocaleLowerCase(); views.library.render(); }, 120);
    });
    $('#view-library .view-toggle').addEventListener('click', (event) => {
      const button = event.target.closest('button');
      if (!button) return;
      libraryFilter.layout = button.dataset.layout;
      views.library.render();
    });
    $('#library-items').addEventListener('click', (event) => {
      const entry = event.target.closest('.cover') && event.target.closest('.media-card')?._entry;
      if (entry) openCollectionEntry(entry);
    });
  },
  render() {
    const container = $('#library-items');
    const libraryPage = data.get('library')?.data;
    const seriesPage = data.get('series')?.data;
    const loaded = loadedItems('library', libraryTail.items);
    const libraryNext = libraryTail.items.length ? libraryTail.next : libraryPage?.next;
    const seriesNext = seriesTail.items.length ? seriesTail.next : seriesPage?.next;
    const button = $('#library-more');
    button.hidden = !libraryNext && !seriesNext;
    const followed = itemsOf(seriesPage).length + seriesTail.items.length;
    button.textContent = libraryNext ? `Load more (${loaded.length} of ${libraryPage.total})` : `Load more series (${followed} of ${seriesPage?.total ?? '?'})`;
    const entries = collection();
    const matches = { '': () => true, movie: (e) => e.type === 'film', series: (e) => e.type === 'show' };
    for (const tab of $('#library-tabs').children) {
      tab.setAttribute('aria-pressed', String(tab.dataset.kind === libraryFilter.kind));
      $('span', tab).textContent = String(entries.filter(matches[tab.dataset.kind]).length);
    }
    for (const toggle of $('#view-library .view-toggle').children) toggle.setAttribute('aria-pressed', String(toggle.dataset.layout === libraryFilter.layout));
    const meta = $('#library-meta');
    if (!entries.length && emptyOrError('library', container, 'Nothing in the library yet. Requests land here once they are ready.')) { meta.hidden = true; return; }
    const shown = entries.filter((e) => matches[libraryFilter.kind](e)
      && (!libraryFilter.query || String(entryRequest(e).title ?? '').toLocaleLowerCase().includes(libraryFilter.query)));
    // The tabs count titles; this line only speaks of a search or of pages not loaded yet.
    meta.textContent = [libraryFilter.query && `${shown.length} ${shown.length === 1 ? 'match' : 'matches'}`,
      libraryNext && `${loaded.length} of ${libraryPage.total} library entries loaded`].filter(Boolean).join(' · ');
    meta.hidden = !meta.textContent;
    const before = Motion.beforeLayout(container);
    container.classList.toggle('list-mode', libraryFilter.layout === 'list');
    if (!shown.length) container.replaceChildren(h('p', { class: 'empty' }, libraryFilter.kind === 'series' && !libraryFilter.query ? 'No series yet. Request a series to follow it.' : 'No loaded title matches.'));
    else { container.querySelector('.empty')?.remove(); reconcile(container, shown, (e) => e.key, createMediaCard, updateCollectionCard); }
    Motion.afterLayout(container, before);
  },
};

/** One key per show: its TMDB id when known, else its title and year. Episodes carry their series' identity. */
const showKey = (request = {}) => (request.tmdb_id != null
  ? `show:tmdb:${request.tmdb_id}`
  : `show:${String(request.title ?? '').trim().toLocaleLowerCase()}:${Number(request.year) || 0}`);
const entryRequest = (entry) => (entry.type === 'film' ? entry.item.request : entry.request) ?? {};

/** Opens a film on its sheet, a show on its seasons. */
export function openCollectionEntry(entry) {
  if (entry.type === 'film') openLibraryEntry(entry.item); else openShow(entry);
}

/** Films stay single cards; episodes gather under their show, beside the series Mynou follows. */
function collection() {
  const shows = new Map();
  const entries = [];
  const showFor = (request) => {
    const key = showKey(request);
    let show = shows.get(key);
    if (!show) { show = { type: 'show', key, request, series: null, episodes: [] }; shows.set(key, show); entries.push(show); }
    return show;
  };
  for (const item of loadedItems('library', libraryTail.items)) {
    if (item.request?.kind === 'episode') showFor(item.request).episodes.push(item);
    else entries.push({ type: 'film', key: item.id, item });
  }
  for (const series of loadedItems('series', seriesTail.items)) {
    const show = showFor(series.request ?? {});
    show.series = series;
    show.request = series.request ?? show.request;
  }
  return entries;
}

function showState(show) {
  if (show.series?.last_error) return ['Needs attention', 'bad'];
  if (show.episodes.some((e) => e.imports_present === false)) return ['File missing', 'bad'];
  if (show.series) return show.series.monitored ? ['Followed', 'ok'] : ['Not followed', 'warn'];
  return ['Episodes requested one by one', 'warn'];
}

function updateCollectionCard(el, entry) {
  el._entry = entry;
  if (entry.type === 'film') { updateLibraryCard(el, entry.item); return; }
  const request = entry.request ?? {};
  const title = request.title ?? 'Untitled series';
  paintCover(el, title, 'Series', request.year);
  coverArt($('.cover', el), { ...request, kind: 'series' });
  $('h3', el).textContent = title;
  $('.year', el).textContent = Number(request.year) ? request.year : '';
  const owned = entry.episodes.length;
  const total = Number(entry.series?.episode_count) || 0;
  $('.quality', el).replaceChildren(
    h('span', {}, total ? `${owned} of ${total} episodes` : `${owned} ${owned === 1 ? 'episode' : 'episodes'}`),
    ...(Number(entry.series?.unmapped_count) ? [h('span', {}, `${entry.series.unmapped_count} to map`)] : []));
  const [text, tone] = showState(entry);
  const state = $('.media-state', el);
  state.textContent = text;
  state.dataset.tone = tone;
}

function openShow(show) {
  const request = show.request ?? {};
  const series = show.series;
  const title = request.title ?? 'Untitled series';
  const tone = coverTone(title);
  const back = () => openShow(show);
  const libraryNext = libraryTail.items.length ? libraryTail.next : data.get('library')?.data?.next;
  const imported = new Map(show.episodes.map((e) => [episodeCode(e.request ?? {}), e]));
  const owned = show.episodes.length;
  const total = Number(series?.episode_count) || 0;
  const [stateText, stateTone] = showState(show);
  const episodes = h('div', { class: 'sheet-seasons' }, series ? skeleton('lines', 6) : importedList(show, back));
  const synopsis = h('div');
  catalogEntry({ ...request, kind: 'series' }).then((found) => {
    if (found?.overview) synopsis.replaceChildren(sheetSection('Synopsis', h('p', { class: 'sheet-synopsis' }, found.overview)));
  });
  const tabs = sheetTabs([
    ['overview', 'Overview', [
      synopsis,
      h('div', { class: 'sheet-highlights' },
        h('div', {}, h('strong', {}, String(owned)), h('span', {}, owned === 1 ? 'episode in your library' : 'episodes in your library')),
        total ? h('div', {}, h('strong', {}, String(total)), h('span', {}, 'episodes in the catalog')) : null,
        Number(series?.unmapped_count) ? h('div', {}, h('strong', {}, String(series.unmapped_count)), h('span', {}, 'to map')) : null),
      sheetSection('Following', sheetFacts(series ? [
        ['Status', series.monitored ? 'Followed: new episodes are requested' : 'Not followed'],
        ['Problem', series.last_error],
        ['Episodes', series.episode_count],
        ['Without a date', Number(series.undated_count) || null],
        ['To map', Number(series.unmapped_count) || null],
        ['Specials', yesNo(series.include_specials)],
        ['From', series.start_date],
        ['Last check', dateTime(series.checked_at)],
        ['Next check', dateTime(series.next_check_at)],
      ] : [['Status', 'Not followed: these episodes were requested one by one']])),
      request.tmdb_id != null ? sheetSection('Quality for this series', titleQuality('series', request.tmdb_id)) : null,
      h('details', { class: 'sheet-diagnostics' }, h('summary', {}, 'Identifiers'),
        sheetFacts([['Series ID', series?.id], ['TMDB ID', request.tmdb_id]], 'mono')),
    ]],
    ['episodes', 'Seasons & episodes', [
      episodes,
      libraryNext ? h('p', { class: 'muted' }, 'Other episodes may be on library pages not loaded yet.') : null,
    ]],
  ]);
  const side = h('div', {},
    sheetPoster(title, tone, artFor({ ...request, kind: 'series' }), Number(request.year) ? String(request.year) : ''),
    h('div', { class: 'sheet-sidebar-info' }, h('span', { class: 'chip', dataset: { tone: stateTone } }, icon(stateTone === 'bad' ? 'alert' : 'check'), stateText)));
  openDetail({ title, tone, side, crumb: ['Your collection', 'Series'], eyebrow: 'Series' },
    h('p', { class: 'detail-sub' }, [Number(request.year) ? request.year : null, total ? `${owned} of ${total} episodes` : `${owned} ${owned === 1 ? 'episode' : 'episodes'}`].filter(Boolean).join(' · ')),
    series?.last_error ? h('p', { class: 'sheet-status', dataset: { tone: 'bad' } }, series.last_error) : null,
    h('div', { class: 'sheet-actions' }, h('button', { type: 'button', class: 'primary', onclick: () => tabs.show('episodes') }, icon('library'), 'Explore the episodes')),
    tabs);
  if (!series) return;
  // Every catalog episode, season by season, with what Mynou holds for it.
  link.call('GET', `/api/series/${encodeURIComponent(series.id)}`).then((record) => {
    const jobs = new Map(jobsLoaded().filter((j) => j.request?.kind === 'episode' && j.state !== 'cancelled'
      && String(j.request?.tmdb_id) === String(request.tmdb_id)).map((j) => [episodeCode(j.request), j]));
    const today = new Date().toISOString().slice(0, 10);
    const seasons = new Map();
    for (const episode of itemsOf(record.episodes)) {
      if (!seasons.has(episode.season)) seasons.set(episode.season, []);
      seasons.get(episode.season).push(episode);
    }
    if (!seasons.size) { episodes.replaceChildren(h('p', { class: 'muted' }, 'The catalog lists no episode yet.')); return; }
    const last = [...seasons.keys()].filter((n) => n > 0).at(-1) ?? [...seasons.keys()][0];
    const picker = h('div', { class: 'segmented season-picker', role: 'group', 'aria-label': 'Season' });
    const panels = [...seasons].map(([number, list]) => {
      const have = list.filter((e) => imported.has(episodeCode(e))).length;
      picker.append(h('button', { type: 'button', 'aria-pressed': String(number === last), dataset: { season: String(number) } },
        number ? `Season ${number}` : 'Specials', h('small', {}, `${have}/${list.length}`)));
      return h('ol', { class: 'activity-list', hidden: number !== last, dataset: { season: String(number) } }, list.map((episode) =>
        episodeRow(series, episode, imported.get(episodeCode(episode)), jobs.get(episodeCode(episode)), today, back)));
    });
    picker.addEventListener('click', (event) => {
      const button = event.target.closest('button');
      if (!button) return;
      for (const b of picker.children) b.setAttribute('aria-pressed', String(b === button));
      for (const panel of panels) panel.hidden = panel.dataset.season !== button.dataset.season;
    });
    episodes.replaceChildren(h('div', { class: 'season-toolbar' }, picker), ...panels);
    Motion.arrive(panels.filter((p) => !p.hidden).flatMap((p) => [...p.children]));
  }).catch((error) => episodes.replaceChildren(h('p', { class: 'error' }, error.message)));
}

/** Episodes imported for a show Mynou does not follow. */
function importedList(show, back) {
  const episodes = show.episodes.toSorted((a, b) =>
    (Number(a.request?.season) - Number(b.request?.season)) || (Number(a.request?.episode) - Number(b.request?.episode)));
  if (!episodes.length) return h('p', { class: 'muted' }, 'No episode has been imported yet.');
  return h('ol', { class: 'activity-list' }, episodes.map((episode) => {
    const [text, tone] = libraryState(episode);
    return h('li', { class: 'row' },
      h('span', { class: 'row-state', dataset: { tone } }, icon(tone === 'bad' ? 'alert' : 'check')),
      h('button', { type: 'button', class: 'open', onclick: () => openLibraryEntry(episode, { label: 'All episodes', run: back }) },
        h('span', { class: 'title' }, episodeCode(episode.request)), text ? h('span', { class: 'sub' }, text) : null),
      quality(qualityOf(episode.release?.title)));
  }));
}

/** One catalog episode: imported, on its way, missing or not aired, with a manual search when it is not imported. */
function episodeRow(series, episode, item, job, today, back) {
  const code = episodeCode(episode);
  const aired = episode.air_date && episode.air_date <= today;
  let state;
  let tone;
  let mark;
  if (item) { [state, tone] = libraryState(item); state ||= 'In your library'; mark = tone === 'bad' ? 'alert' : 'check'; }
  else if (job) {
    state = job.state === 'wanted' ? `${label('job', job.state)} · ${wantedNote(job)}`
      : job.last_error ? `${label('job', job.state)} · ${job.last_error}` : label('job', job.state);
    tone = jobTone(job.state); mark = JOB_ICONS[job.state] ?? 'clock';
  }
  else if (!aired) { state = episode.air_date ? `Airs ${episode.air_date}` : 'Not aired yet'; tone = ''; mark = 'clock'; }
  else { state = episode.monitored === false ? 'Missing · not monitored' : 'Missing'; tone = 'warn'; mark = 'alert'; }
  const open = item ? () => openLibraryEntry(item, { label: 'All episodes', run: back }) : null;
  const sub = [state, episode.air_date && (item || job || aired) ? episode.air_date : null].filter(Boolean).join(' · ');
  return h('li', { class: 'row' },
    h('span', { class: 'row-state', dataset: { tone } }, icon(mark)),
    h(open ? 'button' : 'div', { type: open ? 'button' : undefined, class: 'open', onclick: open ?? undefined },
      h('span', { class: 'title' }, `${code} · ${episode.title ?? ''}`),
      h('span', { class: 'sub', dataset: { tone: tone === 'bad' ? 'bad' : '' } }, sub)),
    item ? quality(qualityOf(item.release?.title))
      : aired ? h('button', { type: 'button', class: 'icon-button', title: 'Choose a release', 'aria-label': `Choose a release for ${code}`,
        onclick: () => openReleases(`${series.request?.title ?? ''} ${code}`, episodeSearch(series.id, episode.season, episode.episode), { label: 'All episodes', run: back }) },
      icon('search')) : null);
}
