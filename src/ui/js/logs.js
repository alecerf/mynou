import { formatBytes, timeOf } from './format.js';
import { $, h, reconcile } from './dom.js';
import { act, skeleton } from './components.js';
import { link } from './link.js';
import { views } from './store.js';

// Logs: what the service wrote to its own log, newest first, at the level
// chosen. The service hides secrets before anything is written.

const STEP = 200;
const MOST = 1000;
const TONES = { error: 'bad', warn: 'warn', info: '', debug: '' };
const LEVELS = { error: 'Error', warn: 'Warning', info: 'Info', debug: 'Detail' };

const state = { level: 'info', limit: STEP, reply: null, painted: null, error: null, loading: false, loaded: false };

// A short local date and the time to the second, as a log reads.
const stamp = (() => {
  const day = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
  const time = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
  return (value) => { const t = timeOf(value); return Number.isNaN(t) ? String(value ?? '') : `${day.format(t)} ${time.format(t)}`; };
})();

async function load() {
  if (state.loading) return;
  state.loading = true;
  state.error = null;
  views.logs.render();
  try {
    state.reply = await link.call('GET', `/api/logs?level=${state.level}&limit=${state.limit}`);
    state.loaded = true;
  } catch (error) {
    state.error = error.message;
  } finally {
    state.loading = false;
    views.logs.render();
  }
}

function row() {
  return h('li', { class: 'log-line' },
    h('time'), h('span', { class: 'badge' }), h('span', { class: 'log-source' }), h('span', { class: 'log-message' }));
}

function meta(reply, shown) {
  const parts = [];
  if (shown) parts.push(`${shown.toLocaleString()} ${shown === 1 ? 'record' : 'records'}, newest first`);
  parts.push(`${formatBytes(Number(reply.bytes) || 0)} kept of at most ${formatBytes(Number(reply.limit_bytes) || 0)}; older records make room for new ones`);
  const dropped = Number(reply.dropped) || 0;
  if (dropped) parts.push(`${dropped.toLocaleString()} could not be written, as the disk refused them`);
  return `${parts.join(' · ')}.`;
}

views.logs = {
  init() {
    $('#logs-refresh').addEventListener('click', (event) => act(event.currentTarget, load));
    $('#logs-level').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-level]');
      if (!button || button.dataset.level === state.level) return;
      state.level = button.dataset.level;
      state.limit = STEP;
      load();
    });
    $('#logs-more').addEventListener('click', (event) => {
      state.limit = Math.min(MOST, state.limit + STEP);
      act(event.currentTarget, load);
    });
  },
  render() {
    for (const button of $('#logs-level').children) button.setAttribute('aria-pressed', String(button.dataset.level === state.level));
    if (!state.loaded && !state.loading && !state.error) { load(); return; }
    const list = $('#logs');
    const more = $('#logs-more');
    if (state.error) {
      list.replaceChildren(h('li', { class: 'empty' }, state.error));
      $('#logs-meta').textContent = '';
      more.hidden = true;
      return;
    }
    if (!state.reply) {
      if (!list.querySelector('.loading')) list.replaceChildren(skeleton('rows', 6));
      more.hidden = true;
      return;
    }
    // Live updates repaint every view: a reply already shown stays as it is.
    if (state.painted === state.reply && list.isConnected && !list.querySelector('.loading')) return;
    state.painted = state.reply;
    const entries = Array.isArray(state.reply.entries) ? state.reply.entries : [];
    $('#logs-meta').textContent = meta(state.reply, entries.length);
    more.hidden = entries.length < state.limit || state.limit >= MOST || Boolean(state.reply.truncated);
    if (!entries.length) {
      list.replaceChildren(h('li', { class: 'empty' }, state.level === 'error' ? 'No error in the log.' : 'Nothing at this level in the log.'));
      return;
    }
    list.querySelector('.empty')?.remove();
    // Records carry no identity: their place, time and text stand for one.
    reconcile(list, entries, (e, i) => `${i}|${e.at}|${e.message}`, row, (el, entry) => {
      const [time, badge, source, message] = el.children;
      time.dateTime = entry.at ?? '';
      time.title = entry.at ?? '';
      time.textContent = stamp(entry.at);
      badge.dataset.tone = TONES[entry.level] ?? '';
      badge.textContent = LEVELS[entry.level] ?? entry.level;
      source.textContent = entry.source ?? '';
      message.textContent = entry.message ?? '';
      el.dataset.level = entry.level ?? '';
    });
  },
  leave() { state.loaded = false; state.reply = null; state.painted = null; state.error = null; state.limit = STEP; },
};
