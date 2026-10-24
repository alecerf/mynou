import { $ } from './dom.js';
import { Motion } from './motion.js';
import { views, currentView, setCurrentView } from './store.js';
import { libraryFilter } from './library.js';

// Views live in the hash: #library, #jobs…

// [id, label, navigation group], in navigation order.
export const VIEWS = [
  ['dashboard', 'Overview', ''],
  ['library', 'Library', ''],
  ['jobs', 'Jobs', 'Activity'],
  ['transfers', 'Transfers', 'Activity'],
  ['settings', 'Settings', ''],
];
// A request is an action reachable from everywhere, not a place in the navigation.
const REQUEST_VIEW = ['requests', 'New request'];

function show(name) {
  // Series now live in the library; old links land on that tab.
  if (name === 'series') { libraryFilter.kind = 'series'; history.replaceState(null, '', '#library'); name = 'library'; }
  // Source states now live beside their settings.
  if (name === 'sources') { history.replaceState(null, '', '#settings'); name = 'settings'; }
  if (!views[name]) name = 'dashboard';
  const changed = currentView() !== name;
  for (const section of document.querySelectorAll('.view')) section.hidden = section.id !== `view-${name}`;
  for (const link of document.querySelectorAll('#nav a, #mobile-nav a, .request-button')) {
    if (link.dataset.view === name) link.setAttribute('aria-current', 'page'); else link.removeAttribute('aria-current');
  }
  const title = [...VIEWS, REQUEST_VIEW].find(([id]) => id === name)[1];
  setCurrentView(name);
  document.title = `${title} · Mynou`;
  views[name].render?.();
  Motion.nav();
  if (changed) { Motion.enter($(`#view-${name}`)); scrollTo({ top: 0, behavior: 'instant' }); }
}

export const route = () => show(location.hash.slice(1));
