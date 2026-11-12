import { $ } from './dom.js';
import { Motion } from './motion.js';
import { views, currentView, currentParam, setCurrentView } from './store.js';
import { libraryFilter } from './library.js';

// Views live in the hash: #library, #jobs…

// [id, label], in navigation order. Settings sits apart at the foot of the sidebar.
export const VIEWS = [
  ['dashboard', 'Overview'],
  ['library', 'Library'],
  ['transfers', 'Transfers'],
  ['jobs', 'Activity'],
  ['settings', 'Settings'],
];
// A request is an action reachable from everywhere, not a place in the navigation.
const REQUEST_VIEW = ['requests', 'New request'];
// Disk space is reached from the library and the transfers, and from the search.
export const STORAGE_VIEW = ['storage', 'Storage'];
// The service's log sits at the foot of the sidebar, above the settings, and in the phone's bottom bar.
export const LOGS_VIEW = ['logs', 'Logs'];

function show(hash) {
  const [base, ...rest] = hash.split('/');
  let name = base;
  let param = '';
  try { param = decodeURIComponent(rest.join('/')); } catch { /* a malformed hash shows the view itself */ }
  // Series now live in the library; old links land on that tab.
  if (name === 'series') { libraryFilter.kind = 'series'; replaceAddress('#library'); name = 'library'; }
  // Source states now live beside their settings.
  if (name === 'sources') { replaceAddress('#settings'); name = 'settings'; }
  if (!views[name]) name = 'dashboard';
  const from = currentView();
  const moved = from !== name;
  const changed = moved || currentParam() !== param;
  if (changed) views[from]?.leave?.();
  for (const section of document.querySelectorAll('.view')) section.hidden = section.id !== `view-${name}`;
  for (const link of document.querySelectorAll('a[data-view]')) {
    if (link.dataset.view === name) link.setAttribute('aria-current', 'page'); else link.removeAttribute('aria-current');
  }
  const title = ORDER.find(([id]) => id === name)[1];
  setCurrentView(name, param);
  document.title = `${title} · Mynou`;
  views[name].render?.();
  Motion.nav();
  // The first page of a visit builds itself (see the motion layer).
  if (!changed || !from) return;
  scrollTo({ top: 0, behavior: 'instant' });
  // A view rises in from below when it sits further down the menu, and comes
  // down from above when it sits higher. Another page of the same view, such
  // as a settings page, enters alone under a title that stays.
  const index = (id) => ORDER.findIndex(([view]) => view === id);
  Motion.enter(moved ? $(`#view-${name}`) : views[name].page?.() ?? $(`#view-${name}`), moved && index(name) < index(from));
}
// Every view in the order it is reached, for titles and the direction of a move.
const ORDER = [...VIEWS.slice(0, -1), REQUEST_VIEW, STORAGE_VIEW, LOGS_VIEW, VIEWS.at(-1)];

// Safari keeps a plain-HTTP page of another machine in an Enhanced Security
// process, and loads the whole page again in a new process for every fragment
// navigation that reaches its navigation policy. One the page intercepts never
// does: links, `location.hash`, going back and the address bar all arrive as
// the `navigate` event of the Navigation API, and the view changes in place.

let shown = null;   // the address the view on screen was shown for

export function route() {
  shown = location.hash;
  show(location.hash.slice(1));
}

/** Shows the view of the address, unless it is on screen already. */
export function follow() {
  if (location.hash !== shown) route();
}

/** Puts `hash` in the address for the view on screen, without showing it again. */
export function replaceAddress(hash) {
  shown = hash;
  history.replaceState(null, '', hash);
}

/** A view, or what an old link to one now opens; anything else may be an
    element of the page, such as the target of the skip link. */
const isView = (id) => { const base = id.split('/')[0]; return !base || Boolean(views[base]) || base === 'series' || base === 'sources'; };

export function followAddress() {
  // A browser without the Navigation API, and an address the browser changed
  // itself, which the page may not intercept.
  addEventListener('hashchange', follow);
  if (!('navigation' in window)) return;
  navigation.addEventListener('navigate', (event) => {
    if (!event.hashChange || !event.canIntercept || event.downloadRequest !== null) return;
    const id = new URL(event.destination.url).hash.slice(1);
    const target = isView(id) ? null : document.getElementById(id);
    if (target) {
      if (event.cancelable) { event.preventDefault(); target.focus(); }
      return;
    }
    // Focus and scrolling stay as the view leaves them.
    event.intercept({ focusReset: 'manual', scroll: 'manual', handler: async () => follow() });
  });
}
