import { TERMINAL_STATES, WORKING, label, formatBytes, relativeTime, duration, JOB_ICONS, toneOf, qualityOf, mediaLabel, mediaLine, fractionOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress, setWorking } from './motion.js';
import { quality, act, skeleton } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, titleOf, byUpdated, transferOf, transferSizes, rateOf } from './store.js';
import { plexError, sourceProblems } from './sources.js';
import { createJobRow, updateJobRow, wireJobRows, openJob } from './jobs.js';
import { createMediaCard, updateLibraryCard, openLibraryEntry } from './library.js';

// Overview: alerts, what is on its way, what was collected and what finished.

// Jobs on their way come first by how far along they are.
const MOTION_ORDER = { downloading: 0, importing: 1, imported: 1, processing: 2, queued: 3 };

views.dashboard = {
  init() {
    wireJobRows($('#recent'));
    $('#in-motion').addEventListener('click', (event) => {
      const card = event.target.closest('[data-key]');
      if (card?._job) openJob(card._job);
      else if (card?.dataset.key === 'queue') location.hash = '#jobs';
    });
    $('#fresh').addEventListener('click', (event) => {
      const card = event.target.closest('.media-card');
      if (event.target.closest('.cover') && card?._item) openLibraryEntry(card._item);
    });
  },
  render() {
    const hour = new Date().getHours();
    $('#greeting').textContent = `The ${hour < 5 ? 'night' : hour < 12 ? 'morning' : hour < 18 ? 'afternoon' : hour < 23 ? 'evening' : 'night'}`;
    const status = data.get('status')?.data;
    const stats = $('#stats');
    if (!status) {
      if (!stats.querySelector('.loading')) stats.replaceChildren(skeleton('lines', 3));
    } else {
      stats.querySelector('.empty')?.remove();
      const cards = [['ready to watch', status.ready], ['on their way', status.active], ['series followed', status.monitored_series]];
      reconcile(stats, cards, (c) => c[0],
        () => h('div', { class: 'stat' }, h('dt'), h('dd')),
        (el, [name, value]) => {
          const dd = $('dd', el);
          const text = String(value ?? '–');
          // A count that moves rises into place; the first paint does not.
          if (dd.textContent && dd.textContent !== text) Motion.tick(dd);
          dd.textContent = text;
          $('dt', el).textContent = name;
        });
      const alerts = [status.maintenance_error && `Maintenance: ${status.maintenance_error}`,
        status.last_sync_error && `Plex watchlist: ${plexError(status.last_sync_error)}`, ...sourceProblems(), status.stopped && 'The service is stopping.'].filter(Boolean);
      const update = status.update?.staged && h('p', { class: 'notice', role: 'status' },
        `Mynou ${status.update.staged} is ready. It installs when Mynou is idle; your data is backed up first. `,
        h('button', { type: 'button', onclick: (e) => act(e.currentTarget, () => link.call('POST', '/api/update/install', {}), 'Installing the update; Mynou restarts') }, 'Install now'));
      // The alerts are rebuilt only when their words change, so a live push
      // neither replays their entrance nor drops a button being pressed.
      const signature = JSON.stringify([alerts, status.update?.staged ?? null]);
      const box = $('#alerts');
      if (box.dataset.signature !== signature) {
        box.dataset.signature = signature;
        box.replaceChildren(...alerts.map((a) => h('p', { class: 'alert', role: 'alert' }, icon('alert'), a)), ...(update ? [update] : []));
        for (const el of box.children) Motion.animate(el, [{ opacity: 0.4, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
      }
    }

    const jobs = itemsOf(data.get('jobs')?.data);
    if (!data.get('jobs')) {
      const root = $('#in-motion');
      if (!root.querySelector('.loading')) root.replaceChildren(h('div', { class: 'empty loading motion-bone', 'aria-busy': 'true' }, h('span', { class: 'visually-hidden' }, 'Loading…'), h('span', { class: 'bone' })));
      if (!$('#recent .loading')) $('#recent').replaceChildren(skeleton('rows', 3));
    } else this.paintMotion(jobs.filter((j) => j.state in MOTION_ORDER)
      .toSorted((a, b) => MOTION_ORDER[a.state] - MOTION_ORDER[b.state] || fractionOf(b.progress) - fractionOf(a.progress) || byUpdated(a, b)));

    const fresh = $('#fresh');
    const library = data.get('library');
    if (!library) { if (!fresh.querySelector('.loading')) fresh.replaceChildren(skeleton('cards', 3)); }
    else if (library.status >= 400 || !itemsOf(library.data).length) fresh.replaceChildren(h('p', { class: 'empty' }, library.status >= 400 ? library.data?.error ?? 'The library is unavailable.' : 'Nothing in the library yet.'));
    else { fresh.querySelector('.empty')?.remove(); reconcile(fresh, itemsOf(library.data).slice(0, 3), (item) => item.id, createMediaCard, updateLibraryCard); }

    const recent = $('#recent');
    // What is on its way already has its cards above; this list keeps what finished.
    const latest = jobs.filter((j) => TERMINAL_STATES.has(j.state)).toSorted(byUpdated).slice(0, 6);
    if (!data.get('jobs')) return;
    if (!latest.length) recent.replaceChildren(h('li', { class: 'empty' }, jobs.length ? 'Nothing has finished yet.' : 'No jobs yet. Make a request to get started.'));
    else { recent.querySelector('.empty')?.remove(); const before = Motion.beforeLayout(recent); reconcile(recent, latest, (j) => j.id, createJobRow, updateJobRow); Motion.afterLayout(recent, before); }
  },
  /** One large card for the job furthest along, two smaller ones, then what waits. */
  paintMotion(active) {
    const root = $('#in-motion');
    // The page takes on the glow of what is furthest along.
    const ambient = $('#ambient');
    if (active.length) ambient.dataset.tone = toneOf(active[0].request?.title ?? active[0].id); else delete ambient.dataset.tone;
    if (!active.length) {
      if (!root.querySelector('.empty-panel')) {
        root.querySelector('.loading')?.remove();
        root.replaceChildren(h('div', { class: 'empty-panel' }, icon('logo', 'logo'),
          h('div', {}, h('strong', {}, 'Nothing on its way.'), h('p', {}, 'Mynou is keeping watch. Make a request when you are ready.'))));
      }
      return;
    }
    root.querySelector('.empty-panel, .loading')?.remove();
    const slots = active.slice(0, 3).map((job, i) => ({ type: i ? 'small' : 'feature', job }));
    const waiting = active.slice(3);
    if (waiting.length) slots.push({ type: 'queue', job: waiting[0], more: waiting.length });
    const before = Motion.beforeLayout(root);
    reconcile(root, slots, (s) => (s.type === 'queue' ? 'queue' : `${s.type}:${s.job.id}`), createMotionCard, updateMotionCard);
    Motion.afterLayout(root, before);
  },
};

function createMotionCard(slot) {
  if (slot.type === 'queue') {
    return h('button', { type: 'button', class: 'motion-card queue-card' },
      h('p', { class: 'eyebrow' }, 'Up next'), h('div', {}, h('div', { class: 'card-title' }), h('div', { class: 'card-sub' })));
  }
  return h('button', { type: 'button', class: `motion-card ${slot.type}` },
    h('div', { class: 'tone-art' }),
    h('div', { class: 'card-top' }, h('span', { class: 'card-label' }, icon('down'), h('span'))),
    h('div', {}, h('div', { class: 'card-title' }), h('div', { class: 'card-sub' }),
      h('div', { class: 'card-status' }, h('span', { class: 'card-size' }), h('strong')), progressBar('Progress')));
}

function updateMotionCard(el, slot) {
  const job = slot.job;
  const request = job.request ?? {};
  if (slot.type === 'queue') {
    $('.card-title', el).textContent = request.title ?? titleOf(job);
    $('.card-sub', el).textContent = slot.more > 1 ? `and ${slot.more - 1} more waiting` : label('job', job.state);
    el._job = null;
    return;
  }
  el._job = job;
  el.dataset.tone = toneOf(request.title ?? job.id);
  el.setAttribute('aria-label', `${mediaLabel(request)}, ${label('job', job.state)}`);
  $('.card-label', el).replaceChildren(icon(JOB_ICONS[job.state] ?? 'clock'), label('job', job.state));
  $('.card-title', el).textContent = request.title ?? titleOf(job);
  $('.card-sub', el).replaceChildren(h('span', {}, mediaLine(request)), quality(qualityOf(job.release?.title)));
  const fraction = fractionOf(job.progress);
  const transfer = transferOf(job);
  const { done, total } = transferSizes(transfer);
  const rate = rateOf(transfer);
  $('.card-size', el).textContent = total
    ? [`${formatBytes(Math.min(done, total))} of ${formatBytes(total)}`, rate && `${formatBytes(rate)}/s`, rate && done < total && `${duration((total - done) / rate)} left`].filter(Boolean).join(' · ')
    : job.last_error ?? (relativeTime(job.created_at) ? `Requested ${relativeTime(job.created_at)}` : '');
  const downloading = job.state === 'downloading' || fraction > 0;
  $('strong', el).textContent = downloading ? `${Math.round(fraction * 100)}%` : '';
  const bar = $('.progress', el);
  // Searching and filing have no amount to show, only that they are under way.
  const working = !downloading && WORKING.has(job.state);
  bar.hidden = !downloading && !working;
  if (working) setWorking(bar); else setProgress(bar, `job:${job.id}`, fraction);
}
