import { formatBytes, duration, mediaLabel } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { act, openDetail, skeleton, toast } from './components.js';
import { link } from './link.js';
import { views, itemsOf, titleOf, transferSizes, jobsLoaded } from './store.js';
import { openRemove } from './transfers.js';

// Storage: what Mynou keeps on disk that the owner may no longer need. An
// upgrade deletes the version it replaces; what is left goes only when asked here.

const state = { earlier: null, downloads: null, error: null, loading: false, loaded: false };

const fileName = (path) => String(path ?? '').split('/').pop();

/** Every transfer, page after page: finished ones may sit past the first. */
async function allTransfers() {
  const all = [];
  let after = null;
  for (let pages = 0; pages < 500; pages += 1) {
    const page = await link.call('GET', `/api/transfers?limit=200${after ? `&after=${encodeURIComponent(after)}` : ''}`);
    all.push(...itemsOf(page.transfers));
    after = page.next;
    if (!after) break;
  }
  return all;
}

async function load() {
  if (state.loading) return;
  state.loading = true;
  state.error = null;
  views.storage.render();
  try {
    const [earlier, transfers] = await Promise.all([link.call('GET', '/api/library/earlier'), allTransfers()]);
    state.earlier = earlier;
    // Finished downloads: complete, not a check of files already here.
    state.downloads = transfers.filter((t) => t.ready && !t.seed_only)
      .map((t) => ({ ...t, bytes: transferSizes(t).total || Number(t.total_bytes) || 0 }))
      .toSorted((a, b) => b.bytes - a.bytes);
    state.loaded = true;
  } catch (error) {
    state.error = error.message;
  } finally {
    state.loading = false;
    views.storage.render();
  }
}

function openDeleteEarlier(entry) {
  const files = itemsOf(entry.earlier_files);
  const remove = h('button', { type: 'submit', class: 'destructive' }, icon('trash'), `Delete ${formatBytes(entry.bytes)}`);
  const form = h('form', { class: 'sheet' },
    h('p', { class: 'muted' }, 'These files leave the disk for good. The version that replaced them stays in your library.'),
    h('ul', { class: 'file-list' }, files.map((file) => h('li', {}, h('code', {}, file.path), ' · ', formatBytes(file.bytes)))),
    h('div', { class: 'actions' },
      h('button', { type: 'button', class: 'secondary', onclick: () => $('#detail').close() }, 'Keep them'), remove));
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    act(remove, async () => {
      const result = await link.call('POST', `/api/library/${encodeURIComponent(entry.id)}/delete-earlier`, {});
      $('#detail').close();
      toast(`${formatBytes(result.freed_bytes)} freed`);
      load();
    });
  });
  openDetail(`Delete the earlier version of ${mediaLabel(entry.request)}?`, form);
}

function row(kind) {
  return h('li', { class: 'row' },
    h('span', { class: 'row-state', dataset: { tone: '' } }, icon(kind === 'earlier' ? 'library' : 'down')),
    h('div', { class: 'open' }, h('div', { class: 'title' }), h('div', { class: 'sub' })),
    h('div', { class: 'buttons' },
      h('span', { class: 'badge' }),
      h('button', { type: 'button', class: 'icon-button danger', dataset: { act: 'delete' } }, icon('trash'))));
}

function summary(earlierBytes, downloadBytes) {
  const strip = $('#storage-summary');
  const tiles = [
    ['Earlier versions', state.earlier ? formatBytes(earlierBytes) : '…'],
    ['Finished downloads', state.downloads ? formatBytes(downloadBytes) : '…'],
  ];
  strip.replaceChildren(...tiles.map(([name, value]) => h('div', { class: 'stat' }, h('dt', {}, name), h('dd', {}, value))));
}

views.storage = {
  init() {
    $('#storage-refresh').addEventListener('click', (event) => act(event.currentTarget, load));
    $('#storage-earlier').addEventListener('click', (event) => {
      const button = event.target.closest('[data-act="delete"]');
      const entry = button?.closest('li')?._entry;
      if (entry) openDeleteEarlier(entry);
    });
    $('#storage-downloads').addEventListener('click', (event) => {
      const button = event.target.closest('[data-act="delete"]');
      const transfer = button?.closest('li')?._transfer;
      if (transfer) openRemove(transfer, titleOf(transfer), load);
    });
  },
  render() {
    if (!state.loaded && !state.loading && !state.error) { load(); return; }
    const earlierList = $('#storage-earlier');
    const downloadList = $('#storage-downloads');
    if (state.error) {
      earlierList.replaceChildren(h('li', { class: 'empty' }, state.error));
      downloadList.replaceChildren();
      return;
    }
    if (!state.loaded) {
      summary(0, 0);
      earlierList.replaceChildren(skeleton('rows', 2));
      downloadList.replaceChildren(skeleton('rows', 3));
      return;
    }
    const earlier = itemsOf(state.earlier?.earlier);
    const downloads = state.downloads ?? [];
    summary(Number(state.earlier?.bytes) || 0, downloads.reduce((sum, t) => sum + t.bytes, 0));
    if (!earlier.length) earlierList.replaceChildren(h('li', { class: 'empty' }, 'No earlier version is left on disk.'));
    else {
      earlierList.querySelector('.empty')?.remove();
      reconcile(earlierList, earlier, (e) => e.id, () => row('earlier'), (el, entry) => {
        const files = itemsOf(entry.earlier_files);
        $('.title', el).textContent = mediaLabel(entry.request);
        $('.sub', el).textContent = [files.map((file) => fileName(file.path)).join(', '),
          entry.replaced_by_release && `replaced by ${entry.replaced_by_release}`].filter(Boolean).join(' · ');
        $('.badge', el).textContent = formatBytes(entry.bytes);
        const button = $('[data-act="delete"]', el);
        button.setAttribute('aria-label', `Delete the earlier version of ${mediaLabel(entry.request)}`);
        button.title = 'Delete these files';
        el._entry = entry;
      });
    }
    if (state.earlier?.limited) earlierList.append(h('li', { class: 'empty' }, 'Only part of the library was searched; delete some, then look again.'));
    if (!downloads.length) { downloadList.replaceChildren(h('li', { class: 'empty' }, 'No finished download is kept.')); return; }
    downloadList.querySelector('.empty')?.remove();
    const owners = new Map(jobsLoaded().filter((j) => j.download_id).map((j) => [j.download_id, j]));
    reconcile(downloadList, downloads, (t) => t.id, () => row('download'), (el, t) => {
      const owner = owners.get(t.id);
      $('.title', el).textContent = owner ? mediaLabel(owner.request) : titleOf(t);
      const sent = Number(t.uploaded_bytes) || 0;
      $('.sub', el).textContent = [owner && titleOf(t), sent && `${formatBytes(sent)} shared`,
        Number(t.seed_elapsed_secs) && `seeding for ${duration(Number(t.seed_elapsed_secs))}`].filter(Boolean).join(' · ');
      $('.badge', el).textContent = formatBytes(t.bytes);
      const button = $('[data-act="delete"]', el);
      button.setAttribute('aria-label', `Remove the download of ${$('.title', el).textContent}`);
      button.title = 'Remove this download';
      el._transfer = t;
    });
  },
  leave() { state.loaded = false; state.earlier = null; state.downloads = null; state.error = null; },
};
