import { TERMINAL_STATES, WORKING, label, formatBytes, timeOf, relativeTime, dateTime, JOB_ICONS, jobTone, toneOf, qualityOf, mediaLabel, mediaLine, fractionOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress, setWorking } from './motion.js';
import { quality, factList, technical, toast, openDetail, act, skeleton } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, titleOf, emptyOrError, byUpdated, transferOf, transferSizes, jobsTail, jobsLoaded } from './store.js';
import { openReleases, requestSearch } from './releases.js';

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

export async function openJob(job) {
  const request = job.request ?? {};
  const events = h('ol', { class: 'events' }, skeleton('rows', 3));
  const transfer = transferOf(job);
  const { done, total } = transferSizes(transfer);
  // A film or an episode Mynou searches for itself can also take a release chosen by hand.
  const searchable = ['movie', 'episode'].includes(request.kind) && !request.source_url && !request.source_path
    && !job.pack_file && !job.imports?.length && job.state !== 'ready';
  const retry = job.state === 'failed' || job.state === 'cancelled';
  const actions = h('div', { class: 'actions' },
    searchable ? h('button', { type: 'button', onclick: () => openReleases(mediaLabel(request), requestSearch(request), { label: 'Back to the job', run: () => openJob(job) }) },
      icon('search'), 'Choose a release') : null,
    retry ? h('button', { type: 'button', onclick: (e) => act(e.currentTarget, () => link.call('POST', `/api/jobs/${encodeURIComponent(job.id)}/retry`, {}), 'Retry queued') },
      icon('retry'), 'Search again') : null);
  openDetail({ title: request.title ?? titleOf(job), tone: toneOf(request.title ?? job.id) },
    h('p', { class: 'detail-sub' }, [mediaLine(request), label('kind', request.kind)].filter((v, i, all) => v && all.indexOf(v) === i).join(' · ')),
    quality(qualityOf(job.release?.title)),
    actions.childElementCount ? actions : null,
    factList([
      ['Status', label('job', job.state)],
      ['Progress', job.state === 'downloading' ? `${Math.round(fractionOf(job.progress) * 100)}%${total ? ` · ${formatBytes(done)} of ${formatBytes(total)}` : ''}` : null],
      ['Problem', job.last_error],
      ['Attempts', Number(job.attempts) ? job.attempts : null],
      ['Next attempt', !TERMINAL_STATES.has(job.state) && dateTime(job.next_attempt_at)],
      ['Requested', dateTime(job.created_at)],
      ['Updated', dateTime(job.updated_at)],
    ]),
    h('h3', {}, 'Events'), events,
    technical([
      ['Job ID', job.id], ['Release', job.release?.title], ['Profile', job.release?.profile], ['TMDB ID', request.tmdb_id],
      ['Transfer', job.download_id], ['Files', job.files], ['Imports', job.imports], ['Pack file', job.pack_file],
    ]));
  try {
    const list = itemsOf(await link.call('GET', `/api/jobs/${encodeURIComponent(job.id)}/events`));
    events.replaceChildren(...(list.length ? list.map((e) => h('li', {}, h('time', {}, dateTime(e.at)), h('span', {}, [label('job', e.state), e.message].filter(Boolean).join(' · '))))
      : [h('li', { class: 'muted' }, 'No events recorded.')]));
  } catch (e) { events.replaceChildren(h('li', { class: 'error' }, e.message)); }
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
