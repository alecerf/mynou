import { WORKING, label, formatBytes, relativeTime, duration, qualityOf, mediaLabel, episodeCode, fractionOf, toneOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress, setWorking } from './motion.js';
import { act, skeleton, artFor } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, titleOf, byUpdated, transferOf, transferSizes, rateOf } from './store.js';
import { plexError, sourceProblems } from './sources.js';
import { openJob, libraryState } from './jobs.js';
import { paintStats } from './stats.js';

// Overview: a title from the collection, what Mynou is busy with, the figures,
// then either the latest additions or the statistics.

// Jobs on their way come first by how far along they are.
const MOTION_ORDER = { downloading: 0, importing: 1, imported: 1, processing: 2, queued: 3 };

/** Artwork for a title: its tone and name at once, its TMDB poster once read.
    The poster stands upright on a blurred copy of itself, never cropped. */
function artFrame(request, className) {
  const title = request.title ?? '';
  const frame = h('span', { class: `art-frame ${className}`, dataset: { tone: toneOf(title) } },
    h('span', { class: 'art-name', 'aria-hidden': 'true' }, title));
  artFor(request).then((url) => {
    if (!url || !frame.isConnected) return;
    const backdrop = h('img', { class: 'art-backdrop', alt: '', decoding: 'async', src: url });
    const upright = h('img', { class: 'art-poster', alt: '', decoding: 'async', src: url });
    upright.addEventListener('load', () => frame.classList.add('has-art'), { once: true });
    frame.prepend(backdrop, upright);
  });
  return frame;
}

const kindLine = (request) => (request.kind === 'episode'
  ? ['Episode', episodeCode(request)]
  : [Number(request.year) ? String(request.year) : null, label('kind', request.kind)]).filter(Boolean).join(' · ');

let panel = 'collection';

views.dashboard = {
  init() {
    $('#in-motion').addEventListener('click', (event) => {
      const card = event.target.closest('[data-key]');
      if (card?._job) openJob(card._job);
      else if (card?.dataset.key === 'queue') location.hash = '#jobs';
    });
    $('#overview-tabs').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-panel]');
      if (!button || button.dataset.panel === panel) return;
      panel = button.dataset.panel;
      this.showPanel();
      Motion.enter($(`#overview-${panel}`));
    });
    $('#feature').addEventListener('click', (event) => { const card = event.target.closest('.feature'); if (card?._item) openJob(card._item); });
    $('#fresh').addEventListener('click', (event) => { const card = event.target.closest('.pick'); if (card?._item) openJob(card._item); });
    this.showPanel();
  },
  showPanel() {
    for (const button of $('#overview-tabs').children) button.setAttribute('aria-pressed', String(button.dataset.panel === panel));
    $('#overview-collection').hidden = panel !== 'collection';
    $('#overview-stats').hidden = panel !== 'stats';
    if (panel === 'stats') paintStats();
  },
  render() {
    const status = data.get('status')?.data;
    const stats = $('#stats');
    if (!status) {
      if (!stats.querySelector('.loading')) stats.replaceChildren(skeleton('lines', 3));
    } else {
      stats.querySelector('.empty')?.remove();
      const plex = data.get('stats')?.data?.plex;
      const cards = [['Films in Plex', plex?.films], ['Series in Plex', plex?.series], ['Episodes in Plex', plex?.episodes],
        ['Fetched by Mynou', status.library], ['In progress', status.active], ['Series followed', status.monitored_series]];
      reconcile(stats, cards, (c) => c[0],
        () => h('div', { class: 'stat' }, h('dt'), h('dd')),
        (el, [name, value]) => {
          const dd = $('dd', el);
          const text = value === undefined || value === null ? '–' : Number(value).toLocaleString();
          // A figure counts up the first time it is shown; one that moves later rises into place.
          const shown = dd.dataset.value;
          dd.dataset.value = text;
          if (shown === undefined) Motion.count(dd, Number(value ?? NaN), text);
          else if (shown !== text) { Motion.settle(dd, text); Motion.tick(dd); }
          $('dt', el).textContent = name;
        });
      const active = Number(status.active) || 0;
      $('#now-count').textContent = active ? `${active} on ${active === 1 ? 'its' : 'their'} way` : 'All quiet';
      $('#now-count').dataset.tone = active ? 'accent' : '';
      const alerts = [status.maintenance_error && `Maintenance: ${status.maintenance_error}`,
        status.last_sync_error && `Plex watchlist: ${plexError(status.last_sync_error)}`, status.earlier_kept, ...sourceProblems(), status.stopped && 'The service is stopping.'].filter(Boolean);
      const paused = status.downloads_enabled === false && h('p', { class: 'notice', role: 'status' },
        'Downloads are off: requests wait until you turn them on in ', h('a', { href: '#settings/downloads' }, 'Settings, Downloads'), '.');
      const update = status.update?.staged && h('p', { class: 'notice', role: 'status' },
        `Mynou ${status.update.staged} is ready. It installs when Mynou is idle; your data is backed up first. `,
        h('button', { type: 'button', onclick: (e) => act(e.currentTarget, () => link.call('POST', '/api/update/install', {}), 'Installing the update; Mynou restarts') }, 'Install now'));
      // The alerts are rebuilt only when their words change, so a live push
      // neither replays their entrance nor drops a button being pressed.
      const signature = JSON.stringify([alerts, status.update?.staged ?? null, status.downloads_enabled]);
      const box = $('#alerts');
      if (box.dataset.signature !== signature) {
        box.dataset.signature = signature;
        box.replaceChildren(...alerts.map((a) => h('p', { class: 'alert', role: 'alert' }, icon('alert'), a)), ...(paused ? [paused] : []), ...(update ? [update] : []));
        for (const el of box.children) Motion.animate(el, [{ opacity: 0.4, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
      }
    }

    if (panel === 'stats') paintStats();

    const jobs = itemsOf(data.get('jobs')?.data);
    if (!data.get('jobs')) {
      const root = $('#in-motion');
      if (!root.querySelector('.loading')) root.replaceChildren(skeleton('lines', 4));
    } else this.paintMotion(jobs.filter((j) => j.state in MOTION_ORDER)
      .toSorted((a, b) => MOTION_ORDER[a.state] - MOTION_ORDER[b.state] || fractionOf(b.progress) - fractionOf(a.progress) || byUpdated(a, b)));

    this.paintCollection();
  },
  /** The newest title, large, and the three before it. */
  paintCollection() {
    const library = data.get('library');
    const feature = $('#feature');
    const fresh = $('#fresh');
    if (!library) {
      if (!feature.querySelector('.loading')) feature.replaceChildren(h('div', { class: 'empty loading feature-bone', 'aria-busy': 'true' }, h('span', { class: 'visually-hidden' }, 'Loading…'), h('span', { class: 'bone' })));
      if (!fresh.querySelector('.loading')) fresh.replaceChildren(skeleton('cards', 3));
      return;
    }
    const items = library.status >= 400 ? [] : itemsOf(library.data).filter((item) => item.state === 'ready' || item.request);
    if (!items.length) {
      const text = library.status >= 400 ? library.data?.error ?? 'The library is unavailable.' : 'Nothing in the library yet.';
      if (feature.dataset.key !== 'empty') {
        feature.dataset.key = 'empty';
        feature.replaceChildren(h('div', { class: 'feature feature-empty' },
          h('span', { class: 'feature-caption' }, h('span', { class: 'feature-meta' }, 'Your collection'), h('strong', {}, 'The first title is one request away.'),
            h('span', {}, text)),
          h('a', { class: 'primary', href: '#requests' }, icon('requests'), 'New request')));
      }
      fresh.replaceChildren(h('p', { class: 'empty' }, 'What Mynou adds to your library shows up here.'));
      return;
    }
    const [first, ...rest] = items;
    if (feature.dataset.key !== first.id) {
      feature.dataset.key = first.id;
      feature.replaceChildren(featureCard(first));
    }
    feature.firstElementChild._item = first;
    const picks = rest.slice(0, 3);
    if (!picks.length) { fresh.replaceChildren(h('p', { class: 'empty' }, 'The titles added before this one show up here.')); return; }
    fresh.querySelector('.empty, .loading')?.remove();
    reconcile(fresh, picks, (item) => item.id, pickCard, (el, item) => {
      el._item = item;
      const [text] = libraryState(item);
      $('.pick-sub', el).textContent = text || qualityOf(item.release?.title).join(' · ') || 'In your library';
    });
  },
  /** A card for each of the three jobs furthest along, then one for what waits. */
  paintMotion(active) {
    const root = $('#in-motion');
    if (!active.length) {
      if (!root.querySelector('.empty-panel')) {
        root.replaceChildren(h('div', { class: 'empty-panel' },
          h('div', {}, h('strong', {}, 'Nothing in progress'), h('p', {}, 'What you request shows up here while Mynou finds and downloads it.')),
          h('a', { class: 'secondary', href: '#requests' }, icon('requests'), 'New request')));
      }
      return;
    }
    root.querySelector('.empty-panel, .loading')?.remove();
    const slots = active.slice(0, 3).map((job) => ({ type: 'job', job }));
    const waiting = active.slice(3);
    if (waiting.length) slots.push({ type: 'queue', job: waiting[0], more: waiting.length });
    const before = Motion.beforeLayout(root);
    reconcile(root, slots, (s) => (s.type === 'queue' ? 'queue' : `${s.type}:${s.job.id}`), createMotionCard, updateMotionCard);
    Motion.afterLayout(root, before);
  },
};

function featureCard(item) {
  const request = item.request ?? {};
  const title = request.title ?? titleOf(item);
  return h('button', { type: 'button', class: 'feature', 'aria-label': `Open ${mediaLabel(request) || title}` },
    artFrame(request, 'feature-art'),
    h('span', { class: 'feature-shade' }),
    h('span', { class: 'feature-tag' }, h('i'), 'Recently added'),
    h('span', { class: 'feature-caption' },
      h('span', { class: 'feature-meta' }, kindLine(request)),
      h('strong', {}, title),
      h('span', {}, qualityOf(item.release?.title).join(' · ') || label('job', item.state))),
    h('span', { class: 'feature-open' }, icon('next')));
}

function pickCard(item) {
  const request = item.request ?? {};
  const title = request.title ?? titleOf(item);
  const traits = qualityOf(item.release?.title);
  return h('button', { type: 'button', class: 'pick' },
    h('span', { class: 'pick-art' }, artFrame(request, 'pick-frame'),
      traits.length ? h('span', { class: 'pick-quality' }, traits[0]) : null,
      h('span', { class: 'pick-open' }, icon('next'))),
    h('span', { class: 'pick-meta' }, kindLine(request)),
    h('span', { class: 'pick-title' }, title),
    h('span', { class: 'pick-sub' }));
}

function createMotionCard(slot) {
  if (slot.type === 'queue') {
    return h('button', { type: 'button', class: 'motion-card queue-card' },
      h('span', { class: 'eyebrow' }, 'Up next'), h('span', { class: 'card-title' }), h('span', { class: 'card-sub' }));
  }
  const request = slot.job.request ?? {};
  return h('button', { type: 'button', class: 'motion-card' },
    artFrame(request, 'motion-art'),
    h('span', { class: 'motion-body' },
      h('span', { class: 'card-top' }, h('span', { class: 'card-title' }), h('strong', { class: 'card-percent' })),
      h('span', { class: 'card-sub' }),
      progressBar('Progress'),
      h('span', { class: 'card-status' })));
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
  el.setAttribute('aria-label', `${mediaLabel(request)}, ${label('job', job.state)}`);
  $('.card-title', el).textContent = request.title ?? titleOf(job);
  const fraction = fractionOf(job.progress);
  const transfer = transferOf(job);
  const { done, total } = transferSizes(transfer);
  const rate = rateOf(transfer);
  $('.card-sub', el).textContent = [request.kind === 'episode' ? episodeCode(request) : Number(request.year) ? request.year : null,
    ...qualityOf(job.release?.title).slice(0, 1), total ? formatBytes(total) : null].filter(Boolean).join(' · ');
  const downloading = job.state === 'downloading' || fraction > 0;
  $('.card-percent', el).textContent = downloading ? `${Math.round(fraction * 100)}%` : '';
  $('.card-status', el).textContent = job.last_error
    ?? (downloading && rate ? [`${formatBytes(rate)}/s`, done < total && `${duration((total - done) / rate)} left`].filter(Boolean).join(' · ')
      : downloading ? label('job', job.state)
      : [label('job', job.state), relativeTime(job.created_at) && `requested ${relativeTime(job.created_at)}`].filter(Boolean).join(' · '));
  $('.card-status', el).dataset.tone = job.last_error ? 'bad' : '';
  const bar = $('.progress', el);
  // Searching and filing have no amount to show, only that they are under way.
  const working = !downloading && WORKING.has(job.state);
  bar.hidden = !downloading && !working;
  if (working) setWorking(bar); else setProgress(bar, `job:${job.id}`, fraction);
}
