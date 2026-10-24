import { h } from './dom.js';
import { toast, skeleton } from './components.js';
import { link } from './link.js';

// What the service sent, the pages read after it, and the selectors over both.

export const data = new Map();      // topic -> { status, data, at }
export const views = {};            // name -> { render(), show() }
let current = '';             // the view on screen
export const currentView = () => current;
export function setCurrentView(name) { current = name; }

export const itemsOf = (value) => {
  if (Array.isArray(value)) return value;
  if (value && typeof value === 'object') {
    const arrays = Object.values(value).filter(Array.isArray);
    return arrays.flat();
  }
  return [];
};
export const titleOf = (item) =>
  item?.request?.title ?? item?.title ?? item?.name ?? item?.release?.title ?? item?.id ?? '(unnamed)';

export function emptyOrError(topic, container, empty) {
  const entry = data.get(topic);
  if (!entry) {
    if (!container.querySelector(':scope > .loading')) container.replaceChildren(skeleton(container.tagName === 'OL' ? 'rows' : 'cards', container.tagName === 'OL' ? 5 : 8));
    return true;
  }
  if (entry.status >= 400) {
    container.replaceChildren(h('p', { class: 'empty' }, entry.data?.error ?? `Unavailable (${entry.status})`));
    return true;
  }
  if (!itemsOf(entry.data).length && empty) { container.replaceChildren(h('p', { class: 'empty' }, empty)); return true; }
  return false;
}

export const byUpdated = (a, b) => (Number(b.updated_at) || 0) - (Number(a.updated_at) || 0);
export const transferOf = (job) => job.download_id && itemsOf(data.get('transfers')?.data).find((t) => t.id === job.download_id);
/** Verified and selected sizes of a transfer, from its file list. */
export function transferSizes(transfer) {
  const files = Array.isArray(transfer?.files) ? transfer.files : [];
  const total = files.filter((f) => f.selected !== false).reduce((sum, f) => sum + (Number(f.size_bytes) || 0), 0);
  return { done: Number(transfer?.verified_bytes) || 0, total };
}

// Download speed is measured here, from the verified bytes of each push; the
// service sends no speed of its own. A transfer that stopped growing for a
// while has no speed.
const samples = new Map();   // transfer id -> { bytes, at, changed, rate }
export function sampleTransfers(page) {
  const at = Date.now();
  for (const transfer of itemsOf(page?.transfers)) {
    const bytes = Number(transfer.verified_bytes) || 0;
    const last = samples.get(transfer.id);
    if (!last || bytes < last.bytes) { samples.set(transfer.id, { bytes, at, changed: at, rate: 0 }); continue; }
    const seconds = (at - last.at) / 1000;
    if (bytes === last.bytes || seconds < 0.5) continue;
    const now = (bytes - last.bytes) / seconds;
    // A long silence gives an average, not a speed: start again from here.
    const rate = seconds > 30 ? 0 : last.rate ? last.rate * 0.7 + now * 0.3 : now;
    samples.set(transfer.id, { bytes, at, changed: at, rate });
  }
}
/** Bytes per second lately, or 0 when unknown or stalled. */
export function rateOf(transfer) {
  const sample = transfer && samples.get(transfer.id);
  return sample && Date.now() - sample.changed < 15000 ? sample.rate : 0;
}

// The live topic carries the newest page; older pages are read on demand, so
// the list is never limited by how many jobs exist.
export const jobsTail = { items: [], next: null };
export const jobsLoaded = () => {
  const first = itemsOf(data.get('jobs')?.data);
  const seen = new Set(first.map((j) => j.id));
  return [...first, ...jobsTail.items.filter((j) => !seen.has(j.id))];
};

export const transfersTail = { items: [], next: null };

/** The first live page followed by the pages loaded after it, without duplicates. */
export function loadedItems(topic, more = []) {
  const first = itemsOf(data.get(topic)?.data);
  const seen = new Set(first.map((item) => item.id));
  return [...first, ...more.filter((item) => !seen.has(item.id))];
}

export const libraryTail = { items: [], next: null };
// Refresh loaded pages in place when jobs outside the first page change.
// A progress update keeps the user's loaded window; membership changes reset it.
export function resetTail(tail) {
  tail.items = []; tail.next = null;
  tail.generation = (tail.generation ?? 0) + 1;
}
export async function refreshTail(topic, tail, first) {
  if (tail.refreshing || !tail.items.length) return;
  tail.refreshing = true;
  const generation = tail.generation ?? 0;
  const wanted = tail.items.length;
  const rows = [];
  let after = first.next;
  try {
    while (after && rows.length < wanted) {
      const page = await link.call('GET', `/api/${topic}?limit=50&after=${encodeURIComponent(after)}`);
      if (page.sequence !== first.sequence) return;
      rows.push(...itemsOf(page[topic]));
      after = page.next;
    }
    if (generation === (tail.generation ?? 0) && data.get(topic)?.data?.sequence === first.sequence) {
      tail.items = rows; tail.next = after;
      if (current === topic) views[topic].render();
    }
  } catch (error) {
    toast(error.message, 'bad');
  } finally {
    tail.refreshing = false;
    const latest = data.get(topic)?.data;
    if (latest && tail.items.length && (latest.sequence !== first.sequence ||
      (topic === 'transfers' && latest.revision !== first.revision))) refreshTail(topic, tail, latest);
  }
}
export function pageMembership(page, topic) {
  return JSON.stringify([itemsOf(page?.[topic]).map((row) => row.id), page?.next]);
}

// The live topic carries the first page; further pages are read on demand, so
// the list is never limited by how many series are tracked.
export const seriesTail = { items: [], next: null };
