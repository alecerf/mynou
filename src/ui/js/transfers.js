import { label, formatBytes, duration, mediaLabel, fractionOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { Motion, progressBar, setProgress } from './motion.js';
import { act } from './components.js';
import { link } from './link.js';
import { data, views, itemsOf, emptyOrError, transferSizes, rateOf, jobsLoaded, transfersTail } from './store.js';

// Transfers: the native torrents, with pause and resume.

views.transfers = {
  init() {
    $('#transfers-more').addEventListener('click', (event) =>
      act(event.currentTarget, async () => {
        if (transfersTail.refreshing) return;
        const after = transfersTail.next ?? data.get('transfers')?.data?.next;
        if (!after) return;
        const generation = transfersTail.generation ?? 0;
        const sequence = data.get('transfers')?.data?.sequence;
        const page = await link.call('GET', `/api/transfers?limit=50&after=${encodeURIComponent(after)}`);
        if (generation !== (transfersTail.generation ?? 0) || sequence !== data.get('transfers')?.data?.sequence || page.sequence !== sequence) return;
        transfersTail.items.push(...itemsOf(page.transfers));
        transfersTail.next = page.next;
        views.transfers.render();
      }));
    $('#transfers').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-act]');
      const row = event.target.closest('.row');
      if (!button || !row?._transfer) return;
      act(button, () => link.call('POST', `/api/transfers/${encodeURIComponent(row._transfer.id)}/${button.dataset.act}`, {}),
        button.dataset.act === 'pause' ? 'Paused' : 'Resumed');
    });
  },
  render() {
    const list = $('#transfers');
    const entry = data.get('transfers')?.data;
    const next = transfersTail.items.length ? transfersTail.next : entry?.next;
    const button = $('#transfers-more');
    button.hidden = !next;
    button.textContent = entry?.total ? `Load more transfers (${itemsOf(entry).length + transfersTail.items.length} of ${entry.total})` : 'Load more transfers';
    if (emptyOrError('transfers', list, 'No native transfers are active.')) return;
    const first = itemsOf(entry);
    const seen = new Set(first.map((item) => item.id));
    // A transfer has no title of its own: the job that started it names it.
    const owners = new Map(jobsLoaded().filter((j) => j.download_id).map((j) => [j.download_id, j]));
    reconcile(list, [...first, ...transfersTail.items.filter((item) => !seen.has(item.id))], (t) => t.id,
      () => h('li', { class: 'row' },
        h('span', { class: 'row-state' }),
        h('div', { class: 'open' }, h('div', { class: 'title' }), h('div', { class: 'sub' })),
        h('div', { class: 'buttons' }, h('span', { class: 'badge' }),
          h('button', { type: 'button', class: 'icon-button', dataset: { act: 'pause' }, 'aria-label': 'Pause', title: 'Pause' }, icon('pause')),
          h('button', { type: 'button', class: 'icon-button', dataset: { act: 'resume' }, 'aria-label': 'Resume', title: 'Resume' }, icon('play'))),
        progressBar('Transfer progress')),
      (el, t) => {
        const fraction = fractionOf(t.progress);
        const owner = owners.get(t.id);
        const firstFile = Array.isArray(t.files) && t.files[0]?.path ? String(t.files[0].path).split('/')[0] : '';
        $('.title', el).textContent = owner ? mediaLabel(owner.request) : firstFile || `Transfer ${String(t.id).slice(0, 8)}`;
        const { done, total } = transferSizes(t);
        const rate = t.paused ? 0 : rateOf(t);
        $('.sub', el).textContent = [`${Math.round(fraction * 100)}%`, total ? `${formatBytes(Math.min(done, total))} of ${formatBytes(total)}` : formatBytes(done),
          rate && `${formatBytes(rate)}/s`, rate && total > done && `${duration((total - done) / rate)} left`, t.message]
          .filter(Boolean).join(' · ');
        const tone = t.failed ? 'bad' : t.ready || t.selected_ready ? 'ok' : t.status === 'downloading' ? 'accent' : 'warn';
        const badge = $('.badge', el);
        badge.textContent = label('transfer', t.status ?? 'queued');
        badge.dataset.tone = tone;
        const state = $('.row-state', el);
        const symbol = t.failed ? 'alert' : t.ready || t.selected_ready ? 'check' : t.paused ? 'pause' : 'down';
        if (state.dataset.state !== symbol) {
          const changed = Boolean(state.dataset.state);
          state.replaceChildren(icon(symbol));
          if (changed) Motion.pop(state);
        }
        Object.assign(state.dataset, { tone, state: symbol });
        setProgress($('.progress', el), `transfer:${t.id}`, fraction);
        $('[data-act="pause"]', el).hidden = Boolean(t.paused);
        $('[data-act="resume"]', el).hidden = !t.paused;
        el._transfer = t;
      });
  },
};
