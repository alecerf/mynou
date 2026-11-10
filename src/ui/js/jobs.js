import { TERMINAL_STATES, WORKING, label, formatBytes, timeOf, relativeTime, dateTime, duration, JOB_ICONS, jobTone, toneOf, qualityOf, qualityFacts, episodeCode, mediaLabel, fractionOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress, setWorking } from './motion.js';
import { quality, toast, openDetail, act, skeleton, sheetPoster, sheetTabs, sheetSection, sheetFacts, copyButton, catalogEntry, artFor } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, titleOf, emptyOrError, byUpdated, transferOf, transferSizes, rateOf, jobsTail, jobsLoaded } from './store.js';
import { openReleases, requestSearch } from './releases.js';
import { titleQuality } from './quality.js';

// Jobs: one row per request, server-side search and the job detail.

export function createJobRow() {
  return h('li', { class: 'row' },
    h('span', { class: 'row-state' }),
    h('button', { class: 'open', type: 'button' }, h('span', { class: 'title' }), h('span', { class: 'sub' })),
    h('time', { class: 'row-time' }),
    h('div', { class: 'buttons' },
      h('button', { type: 'button', class: 'icon-button', dataset: { act: 'retry' }, 'aria-label': 'Retry', title: 'Retry' }, icon('retry')),
      h('button', { type: 'button', class: 'icon-button danger', dataset: { act: 'cancel' }, 'aria-label': 'Cancel', title: 'Cancel' }, icon('close'))),
    progressBar('Download progress'));
}

export function updateJobRow(el, job) {
  const request = job.request ?? {};
  const state = $('.row-state', el);
  if (state.dataset.state !== job.state) {
    const changed = Boolean(state.dataset.state);
    state.dataset.state = job.state;
    state.dataset.tone = jobTone(job.state);
    state.replaceChildren(icon(JOB_ICONS[job.state] ?? 'clock'));
    // A job that moves on under the user's eyes says so.
    if (changed) Motion.pop(state);
  }
  $('.title', el).textContent = request.title ? mediaLabel(request) : titleOf(job);
  const sub = $('.sub', el);
  const progress = job.state === 'downloading' ? ` · ${Math.round(fractionOf(job.progress) * 100)}%` : '';
  sub.textContent = job.last_error ? `${label('job', job.state)} · ${job.last_error}` : `${label('job', job.state)}${progress}`;
  sub.dataset.tone = job.state === 'failed' ? 'bad' : '';
  const time = $('.row-time', el);
  time.textContent = relativeTime(job.updated_at);
  const at = timeOf(job.updated_at);
  if (!Number.isNaN(at)) time.dateTime = new Date(at).toISOString();
  const bar = $('.progress', el);
  bar.hidden = job.state !== 'downloading' && !WORKING.has(job.state);
  if (job.state === 'downloading') setProgress(bar, `job:${job.id}`, fractionOf(job.progress));
  else if (!bar.hidden) setWorking(bar);
  $('[data-act="retry"]', el).hidden = !(job.state === 'failed' || job.state === 'cancelled');
  $('[data-act="cancel"]', el).hidden = TERMINAL_STATES.has(job.state);
  el._job = job;
}

export function wireJobRows(list) {
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

/** What a library entry is now: [text, tone]; no text when all is well. */
export function libraryState(item) {
  if (item.imports_present === false) return ['File missing', 'bad'];
  if (item.pending_upgrade_id) return ['Upgrade on its way', 'accent'];
  if (item.baseline_required || item.group_baseline_required) return ['Release not recorded', 'warn'];
  return ['', 'ok'];
}

/** A job's state as its sheet tells it: [text, tone]. */
function jobStatus(job) {
  if (job.state === 'ready') {
    const [text, tone] = libraryState(job);
    return [text || 'In your library', tone];
  }
  return [label('job', job.state), job.state === 'failed' ? 'bad' : jobTone(job.state)];
}

/** The steps a film or an episode goes through, from what its events recorded. */
function timeline(job, events) {
  const list = h('ol', { class: 'sheet-timeline' });
  const last = events.length - 1;
  list.append(...events.map((event, i) => {
    const current = i === last && !TERMINAL_STATES.has(job.state);
    const bad = event.state === 'failed' || event.state === 'cancelled';
    const mark = current ? String(i + 1) : icon(bad ? 'alert' : 'check');
    return h('li', { class: bad ? 'is-bad' : current ? 'is-current' : 'is-done' },
      h('span', { class: 'timeline-mark' }, mark),
      h('div', {}, h('strong', {}, event.state ? label('job', event.state) : event.message ?? ''), event.state && event.message ? h('p', {}, event.message) : null),
      h('time', { class: 'timeline-state' }, dateTime(event.at)));
  }));
  return list;
}

/** The sheet of one film or episode: what it is, its files and how Mynou got it.
    `back` returns to the sheet it came from, such as its series. */
export async function openJob(job, back) {
  const request = job.request ?? {};
  const title = request.title ?? titleOf(job);
  const tone = toneOf(title);
  const episode = request.kind === 'episode';
  const ready = job.state === 'ready';
  const traits = qualityOf(job.release?.title);
  const [statusText, statusTone] = jobStatus(job);
  const transfer = transferOf(job);
  const { done, total } = transferSizes(transfer);
  const rate = rateOf(transfer);
  const kind = label('kind', request.kind);
  const entry = catalogEntry(request);

  // A film or an episode Mynou searches for itself can also take a release chosen by hand.
  const searchable = ['movie', 'episode'].includes(request.kind) && !request.source_url && !request.source_path
    && !job.pack_file && !job.imports?.length && !ready;
  const retry = job.state === 'failed' || job.state === 'cancelled';
  const again = { label: episode ? mediaLabel(request) : title, run: () => openJob(job, back) };
  const tabs = sheetTabs([
    ['overview', 'Overview', overviewTab(job, request, entry)],
    ['files', 'Files & quality', filesTab(job)],
    ['acquisition', 'Acquisition', acquisitionTab(job, request)],
  ]);
  const actions = h('div', { class: 'sheet-actions' },
    searchable ? h('button', { type: 'button', class: 'primary', onclick: () => openReleases(mediaLabel(request), requestSearch(request), again) },
      icon('search'), 'Choose a release') : null,
    retry ? h('button', { type: 'button', class: searchable ? 'secondary' : 'primary', onclick: (e) => act(e.currentTarget, () => link.call('POST', `/api/jobs/${encodeURIComponent(job.id)}/retry`, {}), 'Retry queued') },
      icon('retry'), 'Search again') : null,
    h('button', { type: 'button', class: 'secondary', onclick: () => tabs.show('acquisition') }, icon('jobs'), 'See the acquisition'));
  const downloading = job.state === 'downloading';
  const progress = downloading ? h('div', { class: 'sheet-progress' },
    progressBar('Download progress'),
    h('p', {}, [`${Math.round(fractionOf(job.progress) * 100)}%`, total && `${formatBytes(Math.min(done, total))} of ${formatBytes(total)}`,
      rate && `${formatBytes(rate)}/s`, rate && total > done && `${duration((total - done) / rate)} left`].filter(Boolean).join(' · '))) : null;
  if (progress) setProgress($('.progress', progress), `sheet:${job.id}`, fractionOf(job.progress));

  const side = h('div', {},
    sheetPoster(title, tone, artFor(request), episode ? episodeCode(request) : Number(request.year) ? String(request.year) : ''),
    h('div', { class: 'sheet-sidebar-info' },
      h('span', { class: 'chip', dataset: { tone: statusTone } }, icon(statusTone === 'bad' ? 'alert' : ready ? 'check' : JOB_ICONS[job.state] ?? 'clock'), statusText),
      traits.length ? quality(traits) : null));
  openDetail({ title, tone, side, crumb: [ready ? 'Your collection' : 'Activity', kind], eyebrow: [kind, episode ? episodeCode(request) : null].filter(Boolean).join(' · ') },
    back ? h('button', { type: 'button', class: 'text-link back', onclick: back.run }, `← ${back.label}`) : null,
    h('p', { class: 'detail-sub' }, [episode ? null : Number(request.year) ? request.year : null, ...traits].filter(Boolean).join(' · ')),
    // The chip beside the poster gives the state; only a failure needs words here.
    job.last_error ? h('p', { class: 'sheet-status', dataset: { tone: 'bad' } }, job.last_error) : null,
    progress,
    actions,
    tabs);
}

function overviewTab(job, request, entry) {
  const highlights = qualityFacts(job.release?.title).slice(0, 3);
  const synopsis = h('div');
  // The synopsis comes from TMDB, the one entry with this title's id.
  entry.then((found) => { if (found?.overview) synopsis.replaceChildren(sheetSection('Synopsis', h('p', { class: 'sheet-synopsis' }, found.overview))); });
  return [
    synopsis,
    highlights.length ? h('div', { class: 'sheet-highlights' }, highlights.map(([name, value]) => h('div', {}, h('strong', {}, value.split(' · ')[0]), h('span', {}, name)))) : null,
    request.kind === 'movie' && request.tmdb_id != null ? sheetSection('Quality for this film', titleQuality('movie', request.tmdb_id)) : null,
    // The title, year and kind head the sheet already; this says what happened to it.
    sheetSection('About this title', sheetFacts([
      ['Upgrades', job.state === 'ready' ? (job.monitored ? 'Watched for a better release' : 'Not watched') : null],
      ['Last upgrade check', job.monitored && dateTime(job.monitor_checked_at)],
      ['Requested', dateTime(job.created_at)],
      [job.state === 'ready' ? 'Added' : 'Updated', dateTime(job.updated_at)],
    ])),
  ];
}

// Import notes are "kind: sentence"; the kind picks the label.
const NOTE_LABELS = {
  plex_folder: 'Folder', folder_kept: 'Folder', folder_fallback: 'Folder',
  runtime_checked: 'Length', runtime_check_skipped: 'Length',
  subtitle_kept: 'Subtitle', subtitle_skipped: 'Subtitle',
};
function importNote(note) {
  const [kind, ...rest] = String(note).split(': ');
  const text = rest.join(': ') || kind;
  return [NOTE_LABELS[kind] ?? 'Note', text.charAt(0).toUpperCase() + text.slice(1)];
}

function filesTab(job) {
  const imports = itemsOf(job.imports);
  const subtitles = imports.length ? itemsOf(job.subtitles) : [];
  const notes = itemsOf(job.import_notes);
  const downloaded = itemsOf(job.files);
  const file = (path, tag, chip) => h('article', { class: 'sheet-file' },
    h('div', { class: 'sheet-file-title' }, h('span', { class: 'badge' }, tag), h('strong', {}, String(path).split('/').at(-1)), chip ? h('span', { class: 'chip' }, chip) : null),
    h('code', {}, path),
    h('div', { class: 'sheet-file-actions' }, copyButton(path)));
  const ext = (path) => (String(path).match(/\.([a-z0-9]{2,4})$/i)?.[1] ?? 'file').toUpperCase();
  return [
    sheetSection('Quality', sheetFacts([...qualityFacts(job.release?.title), ['Release', job.release?.title], ['Profile', job.release?.profile]])
      ?? h('p', { class: 'muted' }, 'No release recorded yet: its quality appears once Mynou picks one.')),
    sheetSection(imports.length ? 'In your library' : 'Files',
      ...(imports.length ? [...imports.map((path) => file(path, ext(path), job.imports_present === false ? 'Missing' : 'Current version')),
        ...subtitles.map((path) => file(path, ext(path), 'Subtitle'))]
        : downloaded.length ? downloaded.map((path) => file(path, ext(path), 'Downloaded'))
        : [h('p', { class: 'muted' }, 'No file yet. The files appear here once the download is imported.')]),
      job.pack_file ? h('p', { class: 'muted' }, `From the pack file ${job.pack_file}`) : null),
    notes.length ? sheetSection('How it was filed', sheetFacts(notes.map(importNote))) : null,
  ];
}

function acquisitionTab(job, request) {
  const steps = h('div', {}, skeleton('lines', 3));
  link.call('GET', `/api/jobs/${encodeURIComponent(job.id)}/events`).then((answer) => {
    const events = itemsOf(answer);
    steps.replaceChildren(events.length ? timeline(job, events) : h('p', { class: 'muted' }, 'No events recorded.'));
  }, (error) => steps.replaceChildren(h('p', { class: 'error' }, error.message)));
  return [
    sheetSection(request.kind === 'episode' ? 'How this episode arrived' : 'How this film arrived', steps),
    sheetSection('Release and selection', sheetFacts([
      ['Release', job.release?.title], ['Profile', job.release?.profile],
      ['Problem', job.last_error], ['Refused releases', itemsOf(job.rejected_releases)],
      ['Attempts', Number(job.attempts) ? job.attempts : null],
      ['Next attempt', !TERMINAL_STATES.has(job.state) && dateTime(job.next_attempt_at)],
    ]) ?? h('p', { class: 'muted' }, 'Mynou has not picked a release yet.')),
    h('details', { class: 'sheet-diagnostics' }, h('summary', {}, 'Identifiers and diagnostics'), sheetFacts([
      ['Job ID', job.id], ['TMDB ID', request.tmdb_id], ['Transfer', job.download_id], ['Upgrade job', job.pending_upgrade_id],
      ['Requester account', job.requester?.account_id], ['Pack file', job.pack_file],
    ], 'mono')),
  ];
}

let jobStates = new Set();   // active state filters

// The filter and state run on the server, so they find jobs on pages that are
// not loaded. Each request examines a bounded window of jobs; a search reads a
// few windows at once and "Search further" continues from its cursor. A live
// change can move any job into or out of the search, so the part already
// searched is read again, at most once per second, and replaces the results.
const JOB_STATES = ['queued', 'processing', 'downloading', 'importing', 'ready', 'failed', 'cancelled'];
const SEARCH_WINDOWS = 4;
const jobSearch = {
  query: '', items: [], next: null, scanned: 0, total: null, requests: 0,
  generation: 0, busy: false, refreshing: false, stale: false, timer: 0,
};
const searchStart = () => ({ items: [], next: null, scanned: 0, total: null, requests: 0 });
function jobQuery() {
  const params = new URLSearchParams();
  const needle = $('#job-filter').value.trim();
  if (needle) params.set('q', needle);
  const [state] = jobStates;
  if (state) params.set('state', state);
  return params.toString();
}
// Reads windows into `result` until it holds a page of matches or made
// SEARCH_WINDOWS requests, and in any case until it examined `reach` jobs,
// within `budget` requests. Only the latest read of the latest search is shown.
async function readJobSearch(result, reach, budget, refreshing) {
  const generation = jobSearch.generation;
  Object.assign(jobSearch, { busy: true, refreshing });
  views.jobs.render();
  try {
    let windows = 0;
    do {
      const after = result.next ? `&after=${encodeURIComponent(result.next)}` : '';
      const page = await link.call('GET', `/api/jobs?limit=50&${jobSearch.query}${after}`);
      if (generation !== jobSearch.generation) return;
      result.items.push(...itemsOf(page.jobs));
      result.next = page.next;
      result.scanned += Number(page.scanned) || 0;
      result.total = page.total;
      result.requests += 1;
      windows += 1;
    } while (result.next && windows < budget && (result.scanned < reach || (result.items.length < 50 && windows < SEARCH_WINDOWS)));
    Object.assign(jobSearch, result);
  } catch (error) {
    if (generation === jobSearch.generation) toast(error.message, 'bad');
  } finally {
    if (generation === jobSearch.generation) {
      Object.assign(jobSearch, { busy: false, refreshing: false });
      if (jobSearch.stale) refreshJobSearch();
      views.jobs.render();
    }
  }
}
async function searchJobs(more) {
  if (!more) {
    clearTimeout(jobSearch.timer);
    jobSearch.generation += 1;
    Object.assign(jobSearch, { query: jobQuery(), ...searchStart(), busy: false, refreshing: false, stale: false, timer: 0 });
    if (!jobSearch.query) { views.jobs.render(); return; }
    await readJobSearch(searchStart(), 0, SEARCH_WINDOWS, false);
    return;
  }
  if (!jobSearch.next || (jobSearch.busy && !jobSearch.refreshing)) return;
  if (jobSearch.refreshing) {
    // The user's request goes first; the refresh runs again after it.
    jobSearch.generation += 1;
    jobSearch.stale = true;
  }
  const { items, next, scanned, total, requests } = jobSearch;
  await readJobSearch({ items: [...items], next, scanned, total, requests }, 0, SEARCH_WINDOWS, false);
}
export function refreshJobSearch() {
  if (!jobSearch.query) return;
  if (jobSearch.busy) { jobSearch.stale = true; return; }
  if (jobSearch.timer) return;
  jobSearch.timer = setTimeout(() => {
    jobSearch.timer = 0;
    if (!jobSearch.query) return;
    if (jobSearch.busy) { jobSearch.stale = true; return; }
    jobSearch.stale = false;
    // New jobs come first, so the searched part grew by the jobs added since.
    const added = Math.max(0, (Number(data.get('jobs')?.data?.total) || 0) - (Number(jobSearch.total) || 0));
    readJobSearch(searchStart(), jobSearch.scanned + added, jobSearch.requests + SEARCH_WINDOWS, true);
  }, 1000);
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
    $('#job-filter').addEventListener('input', (event) => {
      clearTimeout(typing);
      event.target.parentElement.classList.toggle('busy', Boolean(event.target.value.trim()));
      typing = setTimeout(() => searchJobs(false), 250);
    });
    const states = $('#job-state');
    states.append(...JOB_STATES.map((state) => h('option', { value: state }, label('job', state))));
    states.addEventListener('change', () => { jobStates = new Set(states.value ? [states.value] : []); searchJobs(false); });
  },
  render() {
    const list = $('#jobs');
    const entry = data.get('jobs')?.data;
    const more = $('#jobs-more');
    // The search field turns while the server looks; a background refresh stays quiet.
    $('#job-filter').parentElement.classList.toggle('busy', Boolean(jobSearch.query) && jobSearch.busy && !jobSearch.refreshing);
    if (jobSearch.query) {
      // Live rows replace their stale search copies until the refresh lands;
      // a row that left the selected state is hidden meanwhile.
      const live = new Map(jobsLoaded().map((j) => [j.id, j]));
      const [state] = jobStates;
      const shown = jobSearch.items.map((j) => live.get(j.id) ?? j).filter((j) => !state || j.state === state);
      const searching = jobSearch.busy && !jobSearch.refreshing;
      more.hidden = !jobSearch.next || searching;
      more.textContent = `Search further (${jobSearch.scanned} of ${jobSearch.total ?? '?'} jobs searched)`;
      if (!shown.length) {
        list.replaceChildren(searching ? skeleton('rows', 4) : h('li', { class: 'empty' }, jobSearch.next ? 'No match yet; search further.' : 'No job matches the filter.'));
      } else { list.querySelector('.empty')?.remove(); reconcile(list, shown, (j) => j.id, createJobRow, updateJobRow); }
      return;
    }
    const all = jobsLoaded();
    const next = jobsTail.items.length ? jobsTail.next : entry?.next;
    more.hidden = !next;
    more.textContent = entry?.total ? `Load more jobs (${all.length} of ${entry.total})` : 'Load more jobs';
    if (emptyOrError('jobs', list, 'No jobs yet.')) return;
    const shown = all.toSorted(byUpdated);
    list.querySelector('.empty')?.remove();
    const before = Motion.beforeLayout(list);
    reconcile(list, shown, (j) => j.id, createJobRow, updateJobRow);
    Motion.afterLayout(list, before);
  },
};
