import { $, h, icon } from './dom.js';
import { Motion } from './motion.js';

// Small pieces shared by the views: quality tags, fact lists, toasts, the detail
// dialog, and what the page shows while it waits.

export const quality = (traits) => h('div', { class: 'quality' }, traits.map((t) => h('span', {}, t)));

/** Facts with human labels, skipping empty values: [label, value] pairs. */
export function factList(pairs, className = 'facts') {
  const dl = h('dl', { class: className });
  for (const [term, value] of pairs) {
    if (value === undefined || value === null || value === '' || (Array.isArray(value) && !value.length)) continue;
    dl.append(h('dt', {}, term), h('dd', {}, Array.isArray(value) ? value.join('\n') : String(value)));
  }
  return dl;
}
export const technical = (pairs) => h('details', { class: 'technical' }, h('summary', {}, 'Technical details'), factList(pairs));

export function toast(message, tone = 'ok') {
  const el = h('button', { type: 'button', class: 'toast', dataset: { tone }, title: 'Dismiss' }, icon(tone === 'ok' ? 'check' : 'alert'), h('span', {}, message));
  $('#toasts').append(el);
  Motion.animate(el, [{ opacity: 0, transform: 'translateY(12px) scale(.96)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
  if (tone === 'ok') Motion.cat('success'); else Motion.nudge(el);
  let timer = 0;
  // The toast is gone from the page at once; only its picture fades.
  const dismiss = () => {
    clearTimeout(timer);
    if (!el.isConnected) return;
    const leaving = Motion.animate(el, [{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'translateY(8px) scale(.97)' }], { duration: 160, fill: 'forwards' });
    el.inert = true;
    if (leaving) leaving.finished.then(() => el.remove(), () => el.remove()); else el.remove();
  };
  const wait = () => { clearTimeout(timer); timer = setTimeout(dismiss, tone === 'bad' ? 7000 : 3500); };
  // A pointer resting on it holds it there to be read.
  el.addEventListener('pointerenter', () => clearTimeout(timer));
  el.addEventListener('pointerleave', wait);
  el.addEventListener('click', dismiss);
  wait();
}

/** Marks a control as working: a spinner takes the place of its icon, or
    comes before its label, once the work lasts long enough to be seen. */
export function busy(el) {
  el.setAttribute('aria-busy', 'true');
  const use = el.querySelector(':scope > .icon use');
  const previous = use?.getAttribute('href');
  let added = null;
  const timer = setTimeout(() => {
    if (use) use.setAttribute('href', '#i-spinner');
    else { added = icon('spinner'); el.prepend(added); }
    el.classList.add('busy');
  }, 120);
  return () => {
    clearTimeout(timer);
    el.removeAttribute('aria-busy');
    el.classList.remove('busy');
    if (use) use.setAttribute('href', previous);
    added?.remove();
  };
}

/** Placeholders shaped like what is coming: list rows, covers, or lines.
    The wrapper carries the `empty` class so views clear it like a message. */
export function skeleton(shape, count = 4) {
  const label = h('span', { class: 'visually-hidden' }, 'Loading…');
  if (shape === 'rows') {
    return h('li', { class: 'empty loading', 'aria-busy': 'true' }, label,
      Array.from({ length: count }, (_, i) => h('div', { class: 'skeleton-row' },
        h('span', { class: 'bone dot' }), h('span', { class: 'bone line', dataset: { width: String(i % 3) } }))));
  }
  if (shape === 'cards') {
    return h('div', { class: 'empty loading', 'aria-busy': 'true' }, label,
      Array.from({ length: count }, () => h('div', { class: 'skeleton-card' }, h('span', { class: 'bone cover-bone' }), h('span', { class: 'bone line' }), h('span', { class: 'bone line short' }))));
  }
  return h('div', { class: 'empty loading', 'aria-busy': 'true' }, label,
    Array.from({ length: count }, (_, i) => h('span', { class: 'bone line', dataset: { width: String(i % 3) } })));
}

const detailEyebrow = h('span');
/** Opens the detail sheet. `heading` is a title, or { title, tone, art } for
    a title set on the same tone as its cover, over its poster when given. */
export function openDetail(heading, ...content) {
  const { title, tone, art } = typeof heading === 'object' ? heading : { title: heading };
  $('#detail-title').textContent = title;
  if (tone) $('#detail').dataset.tone = tone; else delete $('#detail').dataset.tone;
  // A data URL set through the CSSOM: no inline style, so the CSP holds.
  const hero = $('#detail .detail-hero');
  if (art) { hero.style.setProperty('--art', `url("${art}")`); $('#detail').dataset.art = ''; }
  else { hero.style.removeProperty('--art'); delete $('#detail').dataset.art; }
  // A way back, when given, sits above the title.
  const back = content.find((c) => c?.classList?.contains('back'));
  $('#detail .dialog-top').firstElementChild.replaceWith(back ?? detailEyebrow);
  $('#detail-body').replaceChildren(...content.filter((c) => c && c !== back));
  const dialog = $('#detail');
  // The sheet itself opens in CSS; a sheet already open turns its page.
  if (!dialog.open) dialog.showModal(); else Motion.swap(dialog);
  $('#detail-title').focus();
}

export async function act(button, work, done) {
  button.disabled = true;
  const idle = busy(button);
  try { await work(); if (done) toast(done); } catch (e) { toast(e.message, 'bad'); } finally { idle(); button.disabled = false; }
}
