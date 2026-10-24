// Mynou web interface: no framework, no build step, no dependency.
// Untrusted text only ever reaches the DOM through textContent / text nodes.
// Each part is a native module in js/; this file wires them together.

import { setTheme } from './js/theme.js';
import { $, h, icon } from './js/dom.js';
import { toast } from './js/components.js';
import { Motion } from './js/motion.js';
import { link } from './js/link.js';
import { data, views, currentView, jobsTail, transfersTail, libraryTail, resetTail, refreshTail, pageMembership, sampleTransfers } from './js/store.js';
import { refreshJobSearch } from './js/jobs.js';
import { VIEWS, route } from './js/router.js';
import { setupArt } from './js/art.js';
import { TOKEN_KEY, askToken, firstStart, connectOrSetUp } from './js/session.js';
import './js/dashboard.js';
import './js/transfers.js';
import './js/library.js';
import './js/settings.js';
import './js/requests.js';

function setup() {
  const navLink = ([id, name]) => h('a', { class: 'nav-link', href: `#${id}`, dataset: { view: id } }, icon(id), h('span', {}, name));
  $('#nav').append(...VIEWS.flatMap((view, i) => [view[2] && view[2] !== VIEWS[i - 1]?.[2] ? h('p', { class: 'nav-label' }, view[2]) : null, navLink(view)]).filter(Boolean));
  // On a phone every view sits in the bottom bar; a request starts from the + in the top bar.
  $('#mobile-nav').replaceChildren(...VIEWS.map(navLink));
  addEventListener('resize', () => Motion.nav());
  $('#theme').addEventListener('click', (event) => { const button = event.target.closest('[data-choice]'); if (button) setTheme(button.dataset.choice); });
  for (const view of Object.values(views)) view.init?.();
  $('#detail-close').addEventListener('click', () => $('#detail').close());
  $('#detail').addEventListener('click', (event) => { if (event.target.id === 'detail') event.target.close(); });
  $('#login').addEventListener('cancel', (event) => event.preventDefault());
  $('#login-form').addEventListener('submit', (event) => {
    const token = new FormData(event.currentTarget).get('token').trim();
    $('#login-error').textContent = '';
    event.currentTarget.reset();
    try { sessionStorage.setItem(TOKEN_KEY, token); } catch { /* storage unavailable: the token lives for this page only */ }
    link.connect(token);
  });
  setupArt();
  firstStart();

  const pill = $('#link');
  let lost = false;
  link.addEventListener('state', ({ detail }) => {
    pill.dataset.state = detail;
    $('#link-mobile').dataset.state = detail;
    // The whole page shows a link being sought, on a phone as on a desk.
    document.body.dataset.link = detail;
    if (detail === 'closed' && data.size) lost = true;
    pill.textContent = { open: 'All quiet. All connected.', connecting: 'Connecting…', closed: 'Offline. Reconnecting…' }[detail];
    if (detail === 'open') {
      Motion.cat();
      if (lost) { lost = false; toast('Connected again'); }
      resetTail(jobsTail);
      resetTail(libraryTail);
      resetTail(transfersTail);
      refreshJobSearch();
      $('#login').close();
    }
  });
  link.addEventListener('rejected', () => {
    // A refused handshake before any success usually means a wrong token.
    if (!data.size) { try { sessionStorage.removeItem(TOKEN_KEY); } catch { /* ignore */ } link.disconnect(); askToken('The token was refused. Check it and try again.'); }
  });
  let queued = false;
  link.addEventListener('topic', ({ detail }) => {
    const previous = data.get(detail.topic);
    const tail = detail.topic === 'jobs' ? jobsTail : detail.topic === 'library' ? libraryTail : detail.topic === 'transfers' ? transfersTail : null;
    if (tail && previous && pageMembership(previous.data, detail.topic) !== pageMembership(detail.data, detail.topic)) resetTail(tail);
    data.set(detail.topic, { status: detail.status, data: detail.data, at: Date.now() });
    if (detail.topic === 'transfers') sampleTransfers(detail.data);
    if (detail.topic === 'status' && detail.data?.version) $('#footer-version').textContent = `Mynou ${detail.data.version}`;
    if (detail.topic === 'jobs' && previous?.data?.sequence !== detail.data?.sequence) refreshJobSearch();
    if (tail && (previous?.data?.sequence !== detail.data?.sequence ||
      (detail.topic === 'transfers' && previous?.data?.revision !== detail.data?.revision))) refreshTail(detail.topic, tail, detail.data);
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; views[currentView()]?.render?.(); });
  });
  addEventListener('hashchange', route);
  route();

  let token = '';
  try { token = sessionStorage.getItem(TOKEN_KEY) ?? ''; } catch { /* ignore */ }
  if (token) link.connect(token); else connectOrSetUp();
}

setup();
