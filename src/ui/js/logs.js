import { formatBytes, timeOf } from './format.js';
import { $, h, icon, reconcile } from './dom.js';
import { act, copyText, skeleton, toast } from './components.js';
import { link } from './link.js';
import { views, currentView, currentParam, setCurrentView } from './store.js';

// Logs: what the service writes to its own log, newest first. While the page
// is open it follows the log: the socket sends every new record, and the page
// counts and filters them as the service does. The service hides secrets
// before anything is written.

const STEP = 200;
const MOST = 1000;
// The most bars of the timeline; past it, two bars become one.
const BARS = 60;
const LEVELS = ['error', 'warn', 'info', 'debug'];
const RANK = Object.fromEntries(LEVELS.map((level, index) => [level, index]));
const NAMES = { error: 'Error', warn: 'Warning', info: 'Info', debug: 'Detail' };
const KINDS = { error: ['error', 'errors'], warn: ['warning', 'warnings'], info: ['record of activity', 'records of activity'], debug: ['detail', 'details'] };
// The level as the log file writes it, for a copied record.
const WRITTEN = { error: 'ERROR', warn: 'WARN ', info: 'INFO ', debug: 'DEBUG' };

const state = {
  level: 'info', text: '', source: '', limit: STEP,
  reply: null, shown: null, painted: '', error: null, loading: false, loaded: false, following: false,
  pending: [],   // records pushed while a read is on its way
  again: false,  // records went by unseen during a read
  after: '',     // pushed records not newer than this one were in the read
  held: [],      // new records set aside while the page is paused or read further down
  stale: false,  // more were set aside than the page holds
  paused: false,
  open: new Set(), // repeated records shown one by one
};
// Records carry no identity: each one gets a number as it reaches the page.
let numbered = 0;
let typing = 0;
// Keeps the timeline reaching now, and the day headings right, on a quiet log.
let ticking = 0;

const compact = new Intl.NumberFormat(undefined, { notation: 'compact' });
const clock = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
const minutes = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
const full = new Intl.DateTimeFormat(undefined, { dateStyle: 'full', timeStyle: 'medium' });
const weekday = new Intl.DateTimeFormat(undefined, { weekday: 'long' });
const shortWeekday = new Intl.DateTimeFormat(undefined, { weekday: 'short' });
const shortDay = new Intl.DateTimeFormat(undefined, { weekday: 'short', day: 'numeric' });
const longDate = new Intl.DateTimeFormat(undefined, { weekday: 'long', day: 'numeric', month: 'long' });
const dated = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short' });
const dayOf = (t) => { const d = new Date(t); return `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`; };
const ago = (days) => { const d = new Date(); d.setDate(d.getDate() - days); return dayOf(d); };
const dayName = (() => {
  const far = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'long', year: 'numeric' });
  return (t) => {
    if (dayOf(t) === ago(0)) return 'Today';
    if (dayOf(t) === ago(1)) return 'Yesterday';
    return (new Date(t).getFullYear() === new Date().getFullYear() ? longDate : far).format(t);
  };
})();
/** A moment told briefly: “today 11:30”, “Sunday 11:30”, “9 Nov 11:30”. */
function moment(t) {
  const time = minutes.format(t);
  if (dayOf(t) === ago(0)) return `today ${time}`;
  if (dayOf(t) === ago(1)) return `yesterday ${time}`;
  if (Date.now() - t < 6 * 86400e3) return `${weekday.format(t)} ${time}`;
  return `${dated.format(t)} ${time}`;
}

/** The first moment of the timeline, as short as the axis needs it. */
function axisMoment(t) {
  const time = minutes.format(t);
  if (dayOf(t) === ago(0)) return time;
  if (Date.now() - t < 6 * 86400e3) return `${shortWeekday.format(t)} ${time}`;
  return `${dated.format(t)} ${time}`;
}

const plural = (n, one, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** What a record would show, as the service filters: its source, and the text in its message or source. */
function matches(entry) {
  if (state.source && entry.source !== state.source) return false;
  const text = state.text.toLowerCase();
  return !text || String(entry.message ?? '').toLowerCase().includes(text) || String(entry.source ?? '').toLowerCase().includes(text);
}

const ask = () => [['level', state.level], ['limit', state.limit], ['q', state.text], ['source', state.source]]
  .filter(([, value]) => value !== '')
  .map(([key, value]) => `${key}=${encodeURIComponent(value)}`)
  .join('&');

/** Follows the log while the page is open. The socket subscribes before the
    page reads the log, so no record falls between. */
function follow() {
  const on = currentView() === 'logs';
  if (on === state.following) return;
  state.following = on;
  clearInterval(ticking);
  if (on) {
    link.watch('logs');
    ticking = setInterval(() => views.logs.render(), 30_000);
  } else {
    link.unwatch('logs');
  }
}

// Timeline ----------------------------------------------------------------

/** The timeline of a reply: `start` and `step` in ms, and per bar the
    records of each level. */
function timelineOf(value) {
  const start = timeOf(value?.start);
  const step = Number(value?.step) * 1000;
  if (Number.isNaN(start) || !(step > 0) || !Array.isArray(value.bars)) return null;
  return { start, step, bars: value.bars.map((bar) => LEVELS.map((_, i) => Number(bar?.[i]) || 0)) };
}

/** The bar of the moment `t`, the timeline growing to it: past BARS bars,
    two become one. -1 before the timeline. */
function reach(line, t) {
  for (;;) {
    const index = Math.floor((t - line.start) / line.step);
    if (!(index >= 0)) return -1;
    if (index < BARS) {
      while (line.bars.length <= index) line.bars.push([0, 0, 0, 0]);
      return index;
    }
    const merged = [];
    for (let i = 0; i < line.bars.length; i += 2) merged.push(line.bars[i].map((n, level) => n + (line.bars[i + 1]?.[level] ?? 0)));
    line.bars = merged;
    line.step *= 2;
  }
}

/** The span of bar `i`, told briefly. */
function spanOf(line, i) {
  const from = line.start + i * line.step;
  const to = from + line.step;
  const day = dayOf(from) === ago(0) ? 'Today' : dayOf(from) === ago(1) ? 'Yesterday' : longDate.format(from);
  if (line.step >= 86400e3) return line.step === 86400e3 ? day : `${day} to ${longDate.format(to - 1)}`;
  return `${day}, ${minutes.format(from)} to ${minutes.format(to)}`;
}

/** The counts the chosen level shows, most severe first. */
const shownOf = (bar) => bar.slice(0, RANK[state.level] + 1);

function paintChart(reply) {
  const chart = $('#logs-chart');
  const line = reply?.timeline;
  chart.hidden = !line || !line.bars.length;
  if (chart.hidden) return;
  reach(line, Date.now());
  const bars = line.bars.map(shownOf);
  const sign = `${line.start}|${line.step}|${bars.join(';')}|${dayOf(Date.now())}|${chart.clientWidth}`;
  if (chart.dataset.sign === sign) return;
  chart.dataset.sign = sign;
  const top = Math.max(1, ...bars.map((bar) => bar.reduce((sum, n) => sum + n, 0)));
  const plot = $('#logs-bars');
  const axis = [];
  const daily = line.step >= 86400e3;
  plot.replaceChildren(...bars.map((bar, i) => {
    const sum = bar.reduce((all, n) => all + n, 0);
    const from = line.start + i * line.step;
    // A new day starts in this bar: a faint line, and its name under it.
    const turns = !daily && i > 0 && dayOf(from) !== dayOf(from - line.step);
    if (turns) {
      const mark = h('span', {}, shortDay.format(from + line.step - 1));
      mark.style.left = `${(i / bars.length) * 100}%`;
      axis.push(mark);
    }
    const column = h('span', { class: turns ? 'log-col turns' : 'log-col' });
    if (sum) {
      const stack = h('span', { class: 'log-stack' },
        ...bar.map((n, level) => {
          if (!n) return null;
          const part = h('i', { dataset: { level: LEVELS[level] } });
          part.style.flexGrow = String(n);
          return part;
        }));
      stack.style.height = `max(3px, ${(sum / top) * 100}%)`;
      column.append(stack);
    }
    if (i === Number(plot.dataset.on)) column.classList.add('on');
    return column;
  }));
  const start = h('span', { class: 'edge' }, axisMoment(line.start));
  const end = h('span', { class: 'edge end' }, 'Now');
  $('#logs-axis').replaceChildren(start, ...axis, end);
  // A day's name that would touch another is left out.
  let room = start.offsetLeft + start.offsetWidth + 8;
  for (const mark of axis) {
    if (mark.offsetLeft < room || mark.offsetLeft + mark.offsetWidth + 8 > end.offsetLeft) mark.hidden = true;
    else room = mark.offsetLeft + mark.offsetWidth + 8;
  }
  const errors = line.bars.reduce((sum, bar) => sum + bar[0], 0);
  const warnings = line.bars.reduce((sum, bar) => sum + bar[1], 0);
  chart.setAttribute('role', 'img');
  chart.setAttribute('aria-label', `Records over time, since ${moment(line.start)}: ${plural(errors, 'error')} and ${plural(warnings, 'warning')}.`);
  // The bar under the pointer keeps its tip as the bars change.
  if (!$('#logs-tip').hidden) tip(Number(plot.dataset.on));
}

/** Shows what bar `i` holds above it. */
function tip(i) {
  const line = state.reply?.timeline;
  const plot = $('#logs-bars');
  const box = $('#logs-tip');
  const bar = line?.bars[i];
  if (!bar) { box.hidden = true; return; }
  for (const column of plot.querySelectorAll('.on')) column.classList.remove('on');
  plot.children[i]?.classList.add('on');
  plot.dataset.on = String(i);
  const shown = shownOf(bar);
  const keys = shown.map((n, level) => (n ? h('span', { class: 'key', dataset: { level: LEVELS[level] } }, plural(n, ...KINDS[LEVELS[level]])) : null)).filter(Boolean);
  box.replaceChildren(h('strong', {}, spanOf(line, i)), ...(keys.length ? keys : [h('span', { class: 'key' }, 'Nothing at this level')]));
  box.hidden = false;
  const width = plot.clientWidth;
  const at = ((i + 0.5) / line.bars.length) * width;
  box.style.left = `${Math.min(Math.max(0, width - box.offsetWidth), Math.max(0, at - box.offsetWidth / 2))}px`;
}

/** Brings the newest record shown in bar `i` into view. */
function jump(i) {
  const line = state.reply?.timeline;
  if (!line?.bars[i]) return;
  const from = line.start + i * line.step;
  const to = from + line.step;
  let passed = false;
  for (const el of $('#logs-items').children) {
    const { entry, group } = el.item ?? {};
    const newest = timeOf(entry?.at);
    if (Number.isNaN(newest)) continue;
    // A row of repeated records spans from its oldest to its newest.
    const oldest = group ? timeOf(group.oldest.at) : newest;
    if (!(oldest < to)) continue;
    if (newest < from) { passed = true; break; }
    el.scrollIntoView({ block: 'center', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'instant' : 'smooth' });
    el.classList.remove('lit');
    requestAnimationFrame(() => el.classList.add('lit'));
    return;
  }
  if (passed || !shownOf(line.bars[i]).some(Boolean)) toast('Nothing at this level then');
  else toast('That is older than the records shown: show older records, or find a word to reach it');
}

// Reading ------------------------------------------------------------------

async function load() {
  if (state.loading) return;
  Object.assign(state, { loading: true, error: null, pending: [], again: false });
  const asked = ask();
  views.logs.render();
  try {
    const reply = await link.call('GET', `/api/logs?${asked}`);
    // A page left meanwhile, or asked something else, reads the log again.
    if (state.following && asked === ask()) {
      reply.entries = Array.isArray(reply.entries) ? reply.entries.filter((entry) => entry && typeof entry === 'object') : [];
      reply.sources = Array.isArray(reply.sources) ? reply.sources : [];
      reply.counts = reply.counts ?? {};
      reply.timeline = timelineOf(reply.timeline);
      // A record already shown keeps its number, so its row stays as it is.
      const known = new Map();
      for (const entry of state.reply?.entries ?? []) {
        const key = signOf(entry);
        if (!known.has(key)) known.set(key, []);
        known.get(key).push(entry.id);
      }
      for (const entry of reply.entries) entry.id = known.get(signOf(entry))?.shift() ?? ++numbered;
      Object.assign(state, { reply, loaded: true, held: [], stale: false, after: typeof reply.newest === 'string' ? reply.newest : '' });
    }
  } catch (error) {
    state.error = error.message;
  } finally {
    state.loading = false;
    const pending = state.pending;
    state.pending = [];
    if (state.following) {
      if (!state.error && (asked !== ask() || state.again)) load();
      else if (state.reply && !state.error && pending.length) take(pending, {});
      else views.logs.render();
    }
  }
}

const signOf = (entry) => `${entry.at}\u0000${entry.level}\u0000${entry.source}\u0000${entry.message}`;

/** Whether the operator reads further down: new records then wait above. */
function reading() {
  const list = $('#logs-items');
  if (!list.isConnected) return false;
  // Above the list: the phone's top bar, or the tools held at the top.
  const bar = $('.log-toolbar');
  const covers = [$('.topbar')?.getBoundingClientRect().bottom ?? 0];
  if (getComputedStyle(bar).position === 'sticky') covers.push(bar.getBoundingClientRect().bottom);
  return list.getBoundingClientRect().top < Math.max(0, ...covers) - 1;
}

/** New records pushed by the socket, oldest first. */
function receive({ detail }) {
  if (detail.topic !== 'logs' || !state.following) return;
  const data = detail.data ?? {};
  const entries = Array.isArray(data.entries) ? data.entries.filter((entry) => entry && typeof entry === 'object') : [];
  // A read on its way: its records count once it is answered.
  if (state.loading) {
    state.pending.push(...entries);
    if (Number(data.missed) > 0 || state.pending.length > MOST) state.again = true;
    return;
  }
  if (!state.reply) return;
  // Records went by faster than the socket could carry them: read the log again.
  if (Number(data.missed) > 0) { load(); return; }
  take(entries, data);
}

/** Counts and shows `entries`, and the size the push tells. */
function take(entries, size) {
  const reply = state.reply;
  const counts = { ...reply.counts };
  const sources = new Map(reply.sources.map((source) => [source.name, Number(source.count) || 0]));
  const line = reply.timeline && { ...reply.timeline, bars: reply.timeline.bars.map((bar) => [...bar]) };
  const fresh = [];
  for (const entry of entries) {
    // The read already holds what was written before it.
    if (state.after) {
      if (String(entry.at ?? '') <= state.after) continue;
      state.after = '';
    }
    sources.set(entry.source, (sources.get(entry.source) ?? 0) + 1);
    if (!matches(entry)) continue;
    counts[entry.level] = (Number(counts[entry.level]) || 0) + 1;
    const t = timeOf(entry.at);
    if (line && RANK[entry.level] !== undefined && !Number.isNaN(t)) {
      const bar = reach(line, t);
      if (bar >= 0) line.bars[bar][RANK[entry.level]] += 1;
    }
    if (!(RANK[entry.level] <= RANK[state.level])) continue;
    entry.id = ++numbered;
    entry.fresh = true;
    fresh.unshift(entry);
  }
  const next = {
    ...reply,
    counts,
    timeline: line,
    sources: [...sources].map(([name, count]) => ({ name, count })).sort((a, b) => (a.name < b.name ? -1 : 1)),
    bytes: Number.isFinite(size.bytes) ? size.bytes : reply.bytes,
    dropped: Number.isFinite(size.dropped) ? size.dropped : reply.dropped,
    archive: size.archive ?? reply.archive,
  };
  if (fresh.length && (state.paused || reading())) {
    state.held = [...fresh, ...state.held];
    if (state.held.length > MOST) { state.held.length = MOST; state.stale = true; }
  } else if (fresh.length) {
    next.entries = [...fresh, ...reply.entries].slice(0, MOST);
    state.limit = Math.min(MOST, Math.max(state.limit, next.entries.length));
  }
  state.reply = next;
  views.logs.render();
}

/** Shows the records set aside. */
function release() {
  if (!state.held.length || !state.reply) return;
  if (state.stale) { load(); return; }
  const entries = [...state.held, ...state.reply.entries].slice(0, MOST);
  state.held = [];
  state.limit = Math.min(MOST, Math.max(state.limit, entries.length));
  state.reply = { ...state.reply, entries };
  views.logs.render();
}

// Rows --------------------------------------------------------------------

const same = (a, b) => a.level === b.level && a.source === b.source && a.message === b.message;

/** The records as rows: a heading for each day, and one row for records
    repeated one after the other, unless it was opened. A row keeps its
    oldest record's number, so it stays the same row as it grows. */
function rows(entries) {
  const items = [];
  const days = new Map();
  let day = '';
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    const t = timeOf(entry.at);
    const key = Number.isNaN(t) ? '' : dayOf(t);
    if (key && key !== day) {
      day = key;
      // A clock set back can bring a day twice.
      const n = (days.get(key) ?? 0) + 1;
      days.set(key, n);
      items.push({ key: `d${key}#${n}`, day: key, t });
    }
    let last = i;
    while (last + 1 < entries.length && same(entries[last + 1], entry) && dayOf(timeOf(entries[last + 1].at)) === key) last++;
    const group = { key: `r${entries[last].id}`, count: last - i + 1, newest: entry, oldest: entries[last] };
    if (group.count > 1 && state.open.has(group.key)) {
      for (let member = i; member <= last; member++) {
        items.push({ key: `e${entries[member].id}`, entry: entries[member], group: member === i ? group : null, open: true });
      }
    } else {
      items.push({ key: group.key, entry, group: group.count > 1 ? group : null, open: false });
    }
    i = last;
  }
  return items;
}

function row(item) {
  if (item.day) return h('li', { class: 'log-day' }, h('strong'), h('span'));
  return h('li', { class: 'log-line', onanimationend: (event) => event.currentTarget.classList.remove('fresh', 'lit') },
    h('time'),
    h('span', { class: 'log-level' }),
    h('button', { type: 'button', class: 'log-source' }),
    h('span', { class: 'log-message' }),
    h('button', { type: 'button', class: 'log-repeat', hidden: true }),
    h('button', { type: 'button', class: 'icon-button log-copy', title: 'Copy this record', 'aria-label': 'Copy this record' }, icon('copy')));
}

// What a message holds that the eye looks for: quoted names, addresses and
// paths, and figures with their unit.
const TOKENS = new RegExp([
  /(“[^”\n]{1,200}”|"[^"\n]{1,200}")/.source,
  /(\b[a-z][\w+.-]*:\/\/[^\s"“”<>]*[^\s"“”<>.,;:!?)])/.source,
  /((?<![\w.:/~-])\/[\w.@%+~-][^\s"“”,;)]*[^\s"“”,;:.)])/.source,
  /(\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b|(?<![\w.#])\d+(?:[.,]\d+)*(?:\s?(?:[KMGT]i?B(?:\/s)?|ms|µs|min|s|h|%))?(?!\w))/.source,
].join('|'), 'gu');
const KIND = ['str', 'path', 'path', 'num'];

/** `text` with the figures, names and paths it holds set apart, and each
    place it holds `needle` marked. */
function rich(text, needle, tokens = true) {
  const spans = [];
  if (tokens) {
    for (const found of text.matchAll(TOKENS)) {
      const kind = KIND[found.slice(1).findIndex(Boolean)];
      spans.push([found.index, found.index + found[0].length, kind]);
    }
  }
  const marks = [];
  const lower = text.toLowerCase();
  // A letter whose lower case is longer would shift every place.
  if (needle && lower.length === text.length) {
    for (let at = lower.indexOf(needle); at !== -1; at = lower.indexOf(needle, at + needle.length)) marks.push([at, at + needle.length]);
  }
  if (!spans.length && !marks.length) return [text];
  const piece = (from, to) => {
    const out = [];
    let at = from;
    for (const [start, end, kind] of spans) {
      if (end <= at) continue;
      if (start >= to) break;
      if (start > at) out.push(text.slice(at, start));
      const stop = Math.min(end, to);
      out.push(h('span', { class: `t-${kind}` }, text.slice(Math.max(start, at), stop)));
      at = stop;
    }
    if (at < to) out.push(text.slice(at, to));
    return out;
  };
  const nodes = [];
  let at = 0;
  for (const [start, end] of marks) {
    if (start > at) nodes.push(...piece(at, start));
    nodes.push(h('mark', {}, ...piece(start, end)));
    at = end;
  }
  if (at < text.length) nodes.push(...piece(at, text.length));
  return nodes;
}

/** One of eight tints, always the same for a source. */
function hueOf(name) {
  let n = 0;
  for (const c of name) n = (n * 31 + c.codePointAt(0)) >>> 0;
  return String(n % 8);
}

function paint(el, item) {
  if (item.day) {
    const [name, date] = el.children;
    const sign = `${item.day}|${dayOf(Date.now())}`;
    if (el.sign === sign) return;
    el.sign = sign;
    name.textContent = dayName(item.t);
    // Today and yesterday say which day they are, too.
    date.textContent = /^(Today|Yesterday)$/.test(name.textContent) ? longDate.format(item.t) : '';
    return;
  }
  const { entry, group, open } = item;
  const sign = `${entry.id}|${group?.count}|${open}|${state.text}|${state.source}`;
  if (el.sign === sign) return;
  el.sign = sign;
  el.item = item;
  const [time, level, source, message, repeat] = el.children;
  const t = timeOf(entry.at);
  time.dateTime = entry.at ?? '';
  if (Number.isNaN(t)) time.replaceChildren(String(entry.at ?? ''));
  else {
    time.title = `${full.format(t)}\n${entry.at}`;
    time.replaceChildren(clock.format(t), h('small', {}, `.${String(new Date(t).getMilliseconds()).padStart(3, '0')}`));
  }
  level.textContent = NAMES[entry.level] ?? entry.level;
  const needle = state.text.toLowerCase();
  const name = String(entry.source ?? '');
  source.replaceChildren(...rich(name, needle, false));
  source.dataset.hue = hueOf(name);
  source.title = state.source ? 'Show every source' : `Show only ${name}`;
  message.replaceChildren(...rich(String(entry.message ?? ''), needle));
  repeat.hidden = !group;
  el.classList.toggle('member', open && !group);
  el.classList.toggle('opened', open && Boolean(group));
  if (group) {
    const span = [group.oldest, group.newest].map((record) => { const at = timeOf(record.at); return Number.isNaN(at) ? record.at : clock.format(at); });
    repeat.textContent = `×${group.count.toLocaleString()}`;
    repeat.setAttribute('aria-expanded', String(open));
    repeat.title = `${group.count.toLocaleString()} times in a row, from ${span[0]} to ${span[1]}. ${open ? 'Show them as one row' : 'Show each one'}`;
  }
  el.dataset.level = entry.level ?? '';
  // A record that came in while the page was open lights up once.
  if (entry.fresh && el.lit !== entry.id) { el.lit = entry.id; el.classList.add('fresh'); }
}

/** A row as the log file writes it, to paste in a message. */
function written({ entry, group, open }) {
  const line = (record) => `${record.at} ${WRITTEN[record.level] ?? record.level} ${record.source}: ${record.message}`;
  return group && !open ? `${line(entry)}\n(${group.count} times in a row since ${group.oldest.at})` : line(entry);
}

// Page --------------------------------------------------------------------

function setLevel(level) {
  state.level = level;
  state.limit = STEP;
  // The level stays in the address, for a reload or a link.
  const param = level === 'info' ? '' : level;
  const hash = param ? `#logs/${param}` : '#logs';
  if (location.hash !== hash) history.replaceState(null, '', hash);
  setCurrentView('logs', param);
}

function setSource(source) {
  if (source === state.source) return;
  state.source = source;
  state.limit = STEP;
  $('#logs-source').value = source;
  load();
}

function setText(text) {
  clearTimeout(typing);
  if (text === state.text) return;
  state.text = text;
  state.limit = STEP;
  load();
}

/** How many records the chosen level shows, more severe ones included. */
const total = (reply, level = state.level) =>
  LEVELS.slice(0, RANK[level] + 1).reduce((sum, name) => sum + (Number(reply.counts[name]) || 0), 0);

function meta(reply) {
  const all = total(reply);
  const parts = [h('span', {}, h('strong', {}, all.toLocaleString()), ` ${all === 1 ? 'record' : 'records'}`)];
  const since = timeOf(reply.oldest);
  if (!Number.isNaN(since)) parts.push(h('span', { title: full.format(since) }, `since ${moment(since)}`));
  const bytes = Number(reply.bytes) || 0;
  const limit = Number(reply.limit_bytes) || 0;
  const disk = h('span', { class: 'log-disk', title: `The log keeps its newest ${formatBytes(limit)}; each day before goes to the archive.` },
    h('span', { class: 'gauge' }), `${formatBytes(bytes)} of ${formatBytes(limit)}`);
  disk.firstChild.style.setProperty('--fill', String(limit ? Math.min(1, bytes / limit) : 0));
  parts.push(disk);
  const days = Number(reply.archive?.days) || 0;
  if (days) {
    parts.push(h('span', { title: `Earlier days are kept compressed for ${Number(reply.archive.limit_days) || 30} days, in at most ${formatBytes(Number(reply.archive.limit_bytes) || 0)}. A report holds them.` },
      `${plural(days, 'earlier day')} archived, ${formatBytes(Number(reply.archive.bytes) || 0)}`));
  }
  return parts;
}

function empty() {
  const filtered = state.text || state.source;
  if (!filtered) {
    return h('li', { class: 'log-empty' }, icon(state.level === 'error' ? 'check' : 'logs'),
      h('p', {}, state.level === 'error' ? 'No error in the log.' : 'Nothing at this level in the log.'));
  }
  const what = [state.text && `with “${state.text}”`, state.source && `from ${state.source}`].filter(Boolean).join(' ');
  return h('li', { class: 'log-empty' }, icon('search'), h('p', {}, `No record ${what} at this level.`),
    h('button', { type: 'button', class: 'secondary small', onclick: clear }, 'Clear the search'));
}

function failed(message) {
  return h('li', { class: 'log-empty failed' }, icon('alert'), h('p', {}, message),
    h('button', { type: 'button', class: 'secondary small', onclick: () => load() }, icon('retry'), h('span', {}, 'Try again')));
}

function clear() {
  clearTimeout(typing);
  $('#logs-search').value = '';
  $('#logs-source').value = '';
  state.text = '';
  state.source = '';
  state.limit = STEP;
  load();
}

function paintSources(reply) {
  const select = $('#logs-source');
  const names = reply.sources.map((source) => source.name);
  if (state.source && !names.includes(state.source)) names.push(state.source);
  select.classList.toggle('chosen', Boolean(state.source));
  // Options change only when a source appears: a list held open stays as it is.
  const sign = names.join('\n');
  if (select.dataset.sign === sign) return;
  select.dataset.sign = sign;
  select.replaceChildren(h('option', { value: '' }, 'All sources'), ...names.map((name) => h('option', { value: name }, name)));
  select.value = state.source;
}

function paintNews() {
  const news = $('#logs-news');
  const n = state.held.length;
  news.hidden = !n;
  const label = !n ? '' : state.stale ? `More than ${MOST.toLocaleString()} new records` : plural(n, 'new record');
  if (news.dataset.label === label) return;
  news.dataset.label = label;
  news.replaceChildren(icon('up'), label);
}

/** Live, paused, or waiting for Mynou: one button tells it and pauses. */
function paintPause() {
  const button = $('#logs-pause');
  const mode = link.state !== 'open' ? 'waiting' : state.paused ? 'paused' : 'live';
  const sign = `${mode}|${state.paused}`;
  if (button.dataset.sign === sign) return;
  button.dataset.sign = sign;
  button.dataset.state = mode;
  button.setAttribute('aria-pressed', String(state.paused));
  button.title = {
    live: 'New records come in at the top as they are written. Pause to keep the list still.',
    paused: 'The list stays still; new records wait above it. Resume to show them.',
    waiting: 'Mynou does not answer: the log follows again once it does.',
  }[mode];
  button.replaceChildren(h('span', { class: 'dot' }), h('span', {}, { live: 'Live', paused: 'Paused', waiting: 'Waiting' }[mode]),
    icon(state.paused ? 'play' : 'pause'));
}

/** The level buttons, each with how many records it shows. */
function paintLevels(reply) {
  const box = $('#logs-level');
  for (const button of box.children) {
    const level = button.dataset.level;
    const pressed = String(level === state.level);
    if (button.getAttribute('aria-pressed') !== pressed) button.setAttribute('aria-pressed', pressed);
    const count = reply ? total(reply, level) : 0;
    const text = reply ? compact.format(count) : '';
    const small = button.querySelector('small');
    if (small.textContent === text) continue;
    small.textContent = text;
    small.title = reply ? plural(count, 'record') : '';
    small.dataset.tone = level === 'error' && count ? 'bad' : '';
  }
}

/** Under the list: how much of it is shown, and how to reach the rest. */
function paintFoot(reply) {
  const foot = $('#logs-foot');
  const more = $('#logs-more');
  const shown = reply?.entries.length ?? 0;
  foot.hidden = !shown;
  if (!shown) return;
  const all = Math.max(total(reply), shown);
  const older = shown < all && (shown >= state.limit || Boolean(reply.truncated));
  let text;
  if (older && reply.truncated) text = `The newest ${shown.toLocaleString()} records fill what one reading holds. Find a word or choose a source to reach older ones; a report holds them all.`;
  else if (older && state.limit >= MOST) text = `The page holds the newest ${MOST.toLocaleString()} records. Find a word or choose a source to reach older ones; a report holds them all.`;
  else if (older) text = `The newest ${shown.toLocaleString()} of ${all.toLocaleString()} records.`;
  else if (state.text || state.source) text = `Every record found: ${all.toLocaleString()}.`;
  else {
    const since = timeOf(reply.oldest);
    const archived = Number(reply.archive?.days) > 0 ? ' Earlier days are in the archive a report holds.' : '';
    text = `${Number.isNaN(since) ? 'The start of the log.' : `The start of the log, ${moment(since)}.`}${archived}`;
  }
  $('#logs-shown').textContent = text;
  more.hidden = !older || reply.truncated || state.limit >= MOST;
}

views.logs = {
  init() {
    $('#logs-level').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-level]');
      if (!button || button.dataset.level === state.level) return;
      setLevel(button.dataset.level);
      load();
    });
    const search = $('#logs-search');
    search.addEventListener('input', () => {
      clearTimeout(typing);
      const text = search.value.trim();
      typing = setTimeout(() => setText(text), 250);
    });
    // Escape empties the search, then leaves it.
    search.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      if (!search.value) { search.blur(); return; }
      search.value = '';
      setText('');
    });
    $('#logs-source').addEventListener('change', (event) => setSource(event.target.value));
    $('#logs-items').addEventListener('click', async (event) => {
      const line = event.target.closest('.log-line');
      if (!line?.item) return;
      if (event.target.closest('.log-source')) { setSource(state.source ? '' : line.item.entry.source); return; }
      if (event.target.closest('.log-repeat')) {
        const { key } = line.item.group;
        if (!state.open.delete(key)) state.open.add(key);
        state.shown = null;
        views.logs.render();
        return;
      }
      if (!event.target.closest('.log-copy')) return;
      try { await copyText(written(line.item)); toast('Record copied'); } catch { toast('This browser does not allow copying here', 'bad'); }
    });
    $('#logs-more').addEventListener('click', (event) => {
      state.limit = Math.min(MOST, state.limit + STEP);
      act(event.currentTarget, load);
    });
    $('#logs-pause').addEventListener('click', () => {
      state.paused = !state.paused;
      if (!state.paused && !reading()) release();
      views.logs.render();
    });
    $('#logs-news').addEventListener('click', () => {
      state.paused = false;
      release();
      scrollTo({ top: 0, behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'instant' : 'smooth' });
    });
    $('#view-logs a[href="/api/report"]').addEventListener('click', () => toast('Preparing the report. Your browser downloads it in a moment.'));
    // The timeline: a tip for the bar under the pointer, a click goes there.
    const plot = $('#logs-bars');
    const barAt = (event) => {
      const n = state.reply?.timeline?.bars.length ?? 0;
      const box = plot.getBoundingClientRect();
      return Math.min(n - 1, Math.max(0, Math.floor(((event.clientX - box.left) / box.width) * n)));
    };
    plot.addEventListener('pointermove', (event) => {
      const i = barAt(event);
      if (String(i) !== plot.dataset.on || $('#logs-tip').hidden) tip(i);
    });
    plot.addEventListener('pointerleave', () => {
      $('#logs-tip').hidden = true;
      delete plot.dataset.on;
      for (const column of plot.querySelectorAll('.on')) column.classList.remove('on');
    });
    plot.addEventListener('click', (event) => jump(barAt(event)));
    // Rows and the new-records pill stay under the tools held at the top.
    new ResizeObserver(([entry]) => {
      $('#view-logs').style.setProperty('--log-bar', `${Math.round(entry.borderBoxSize?.[0]?.blockSize ?? entry.target.offsetHeight)}px`);
    }).observe($('.log-toolbar'));
    // The names of the days under the timeline are placed for its width.
    new ResizeObserver(() => { if (state.reply && !state.error) paintChart(state.reply); }).observe($('#logs-chart'));
    // Back at the newest records, those set aside come in.
    addEventListener('scroll', () => {
      if (state.following && state.held.length && !state.paused && !reading()) release();
    }, { passive: true });
    // `/` finds in the log, as in most log viewers.
    document.addEventListener('keydown', (event) => {
      if (event.key !== '/' || !state.following || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.target.closest?.('input, textarea, select, [contenteditable]') || document.querySelector('dialog[open]')) return;
      event.preventDefault();
      search.focus();
      search.select();
    });
    link.addEventListener('topic', receive);
    link.addEventListener('state', ({ detail }) => {
      if (!state.following) return;
      paintPause();
      // What was written while the link was down comes with a new read, sent
      // once the socket has subscribed again.
      if (detail === 'open' && (state.loaded || state.error)) queueMicrotask(load);
    });
  },
  render() {
    if (!state.following) {
      const level = currentParam();
      if (RANK[level] !== undefined) state.level = level;
    }
    follow();
    if (!state.loaded && !state.loading && !state.error) { load(); return; }
    const list = $('#logs-items');
    list.setAttribute('aria-busy', String(state.loading));
    $('#logs-search').parentElement.classList.toggle('busy', state.loading && Boolean(state.text));
    paintPause();
    paintNews();
    const reply = state.reply;
    paintLevels(reply);
    paintChart(state.error ? null : reply);
    if (state.error) {
      list.replaceChildren(failed(state.error));
      $('#logs-meta').replaceChildren();
      $('#logs-foot').hidden = true;
      state.painted = '';
      state.shown = null;
      return;
    }
    if (!reply) {
      if (!list.querySelector('.loading')) list.replaceChildren(skeleton('rows', 8));
      $('#logs-foot').hidden = true;
      return;
    }
    // Live updates repaint every view: a reply already shown stays as it is,
    // until the day turns.
    const today = dayOf(Date.now());
    if (state.painted === today && state.shown === reply && !list.querySelector('.loading')) return;
    state.painted = today;
    state.shown = reply;
    paintSources(reply);
    $('#logs-meta').replaceChildren(...meta(reply));
    const dropped = Number(reply.dropped) || 0;
    const alert = $('#logs-dropped');
    alert.hidden = !dropped;
    if (dropped) alert.replaceChildren(icon('alert'), `${plural(dropped, 'record')} could not be written: the disk refused them.`);
    paintFoot(reply);
    if (!reply.entries.length) { list.replaceChildren(empty()); return; }
    list.querySelector('.log-empty, .loading')?.remove();
    reconcile(list, rows(reply.entries), (item) => item.key, row, paint);
  },
  leave() {
    clearTimeout(typing);
    clearInterval(ticking);
    if (state.following) link.unwatch('logs');
    $('#logs-search').value = '';
    $('#logs-source').classList.remove('chosen');
    Object.assign(state, {
      following: false, loaded: false, reply: null, shown: null, painted: '', error: null, limit: STEP,
      level: 'info', text: '', source: '', pending: [], held: [], stale: false, paused: false, after: '', open: new Set(),
    });
  },
};
