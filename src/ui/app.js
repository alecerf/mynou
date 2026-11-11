// Mynou web interface: no framework, no build step, no dependency.
// Untrusted text only ever reaches the DOM through textContent / text nodes.
// Each part is a native module in js/; this file wires them together.

import { setTheme } from './js/theme.js';
import { $, h, icon } from './js/dom.js';
import { toast } from './js/components.js';
import { Motion } from './js/motion.js';
import { setupCats, linkMood } from './js/cat.js';
import { link } from './js/link.js';
import { data, views, currentView, jobsTail, transfersTail, libraryTail, resetTail, refreshTail, pageMembership, sampleTransfers, itemsOf } from './js/store.js';
import { refreshJobSearch } from './js/jobs.js';
import { VIEWS, LOGS_VIEW, route } from './js/router.js';
import { setupSession, connect } from './js/session.js';
import './js/dashboard.js';
import './js/transfers.js';
import './js/library.js';
import './js/settings.js';
import './js/requests.js';
import './js/storage.js';
import './js/logs.js';

function setup() {
  const navLink = ([id, name]) => h('a', { class: 'nav-link', href: `#${id}`, dataset: { view: id } }, icon(id), h('span', {}, name),
    id === 'transfers' ? h('small', { class: 'nav-count', hidden: true }) : null);
  $('#nav').append(...VIEWS.filter(([id]) => id !== 'settings').map(navLink));
  // On a phone every view sits in the bottom bar, the logs before the settings
  // as in the sidebar; a request starts from the + in the top bar.
  $('#mobile-nav').replaceChildren(...[...VIEWS.slice(0, -1), LOGS_VIEW, VIEWS.at(-1)].map(navLink));
  addEventListener('resize', () => Motion.nav());
  // The links take their final width once the typeface has arrived.
  document.fonts?.ready.then(() => Motion.nav());
  $('#theme').addEventListener('click', (event) => {
    const button = event.target.closest('[data-choice]');
    if (!button) return;
    const box = button.getBoundingClientRect();
    setTheme(button.dataset.choice, { x: box.left + box.width / 2, y: box.top + box.height / 2 });
  });
  for (const view of Object.values(views)) view.init?.();
  $('#detail-close').addEventListener('click', () => $('#detail').close());
  $('#detail').addEventListener('click', (event) => { if (event.target.id === 'detail') event.target.close(); });

  const pill = $('#link');
  let lost = false;
  link.addEventListener('state', ({ detail }) => {
    pill.dataset.state = detail;
    $('#link-mobile').dataset.state = detail;
    $('#link-pill').dataset.state = detail;
    // The top of the page speaks up only while the link is down.
    $('#link-pill').hidden = detail === 'open';
    $('#link-pill').textContent = { open: '', connecting: 'Connecting…', closed: 'Offline. Reconnecting…' }[detail];
    // The whole page shows a link being sought, on a phone as on a desk.
    document.body.dataset.link = detail;
    linkMood(detail);
    if (detail === 'closed' && data.size) lost = true;
    pill.textContent = { open: 'Online', connecting: 'Connecting…', closed: 'Offline' }[detail];
    if (detail === 'open') {
      if (lost) { lost = false; toast('Connected again'); }
      resetTail(jobsTail);
      resetTail(libraryTail);
      resetTail(transfersTail);
      refreshJobSearch();
    }
  });
  let queued = false;
  link.addEventListener('topic', ({ detail }) => {
    // One transfer's details belong to its open sheet only, new log records to the Logs page.
    if (detail.topic.startsWith('transfer:') || detail.topic === 'logs') return;
    const previous = data.get(detail.topic);
    const tail = detail.topic === 'jobs' ? jobsTail : detail.topic === 'library' ? libraryTail : detail.topic === 'transfers' ? transfersTail : null;
    if (tail && previous && pageMembership(previous.data, detail.topic) !== pageMembership(detail.data, detail.topic)) resetTail(tail);
    data.set(detail.topic, { status: detail.status, data: detail.data, at: Date.now() });
    if (detail.topic === 'transfers') {
      sampleTransfers(detail.data);
      // Transfers still downloading, beside their place in the navigation.
      // The service counts every transfer; a page only holds the first ones.
      const busy = Number.isFinite(detail.data?.active) ? detail.data.active
        : itemsOf(detail.data?.transfers).filter((t) => ['queued', 'downloading'].includes(t.status) && !t.paused && !t.seed_only).length;
      for (const count of document.querySelectorAll('.nav-count')) { count.textContent = String(busy); count.hidden = !busy; }
    }
    if (detail.topic === 'status' && detail.data?.version) {
      const version = `Mynou ${detail.data.version}`;
      if ($('#about-version').textContent !== version) $('#about-version').textContent = version;
    }
    if (detail.topic === 'jobs' && previous?.data?.sequence !== detail.data?.sequence) refreshJobSearch();
    if (tail && (previous?.data?.sequence !== detail.data?.sequence ||
      (detail.topic === 'transfers' && previous?.data?.revision !== detail.data?.revision))) refreshTail(detail.topic, tail, detail.data);
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; views[currentView()]?.render?.(); });
  });
  setupCats();
  Motion.fadeOnScroll($('.welcome'), 22);
  setupSession();
  addEventListener('hashchange', route);
  // The first page builds itself once (see the motion layer); later ones
  // rise in whole (Motion.enter).
  document.documentElement.dataset.boot = '';
  setTimeout(() => delete document.documentElement.dataset.boot, 1400);
  route();
  connect();
}

setup();
