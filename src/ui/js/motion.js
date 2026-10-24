import { $, h } from './dom.js';

// Every DOM change happens first; an animation is a cancellable response to
// it, never a condition for an action. The browser's reduced-motion setting
// turns all of it off.
export const Motion = (() => {
  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  const channels = new WeakMap();
  const progressValues = new Map();
  const ease = 'cubic-bezier(.2,.75,.25,1)';
  function animate(el, frames, options = {}, channel = 'response') {
    if (!el?.animate || reduce.matches) return null;
    let owned = channels.get(el);
    if (!owned) { owned = new Map(); channels.set(el, owned); }
    owned.get(channel)?.cancel();
    const animation = el.animate(frames, { duration: 200, easing: ease, ...options });
    owned.set(channel, animation);
    return animation;
  }
  const enter = (view) =>
    animate(view, [{ opacity: 0.72, transform: 'translateY(7px)' }, { opacity: 1, transform: 'none' }], { duration: 180 }, 'view');
  /** Positions before a change, for `afterLayout` to glide each child from. */
  function beforeLayout(root) {
    const positions = new Map();
    if (reduce.matches || !root.offsetParent || root.children.length > 300) return positions;
    for (const child of root.children) positions.set(child, child.getBoundingClientRect());
    return positions;
  }
  function afterLayout(root, before) {
    if (!before.size) return;
    for (const el of root.children) {
      const after = el.getBoundingClientRect();
      if (after.bottom < 0 || after.top > innerHeight) continue;
      const old = before.get(el);
      if (!old) {
        animate(el, [{ opacity: 0.6, transform: 'translateY(6px)' }, { opacity: 1, transform: 'none' }], { duration: 160 }, 'layout');
        continue;
      }
      const x = old.left - after.left, y = old.top - after.top;
      if (Math.abs(x) + Math.abs(y) < 1) continue;
      // A long move gets a local response, not a journey across the screen.
      if (Math.hypot(x, y) > 350) animate(el, [{ opacity: 0.75, transform: 'translateY(5px)' }, { opacity: 1, transform: 'none' }], { duration: 160 }, 'layout');
      else animate(el, [{ transform: `translate(${x}px, ${y}px)` }, { transform: 'none' }], { duration: 220 }, 'layout');
    }
  }
  /** Sets a progress bar and, when its value changed, glides it from the old one. */
  function progress(bar, key, fraction) {
    const value = Math.min(1, Math.max(0, fraction));
    const fill = bar.firstElementChild;
    const from = progressValues.get(key);
    fill.style.transform = `scaleX(${value})`;
    if (from !== undefined && Math.abs(from - value) > 0.001) {
      animate(fill, [{ transform: `scaleX(${from})` }, { transform: `scaleX(${value})` }], { duration: 360 }, 'progress');
    }
    progressValues.set(key, value);
  }
  function nav() {
    const indicator = $('#nav-indicator');
    const selected = $('#nav [aria-current="page"]');
    if (!indicator) return;
    if (!selected) { delete indicator.dataset.placed; return; }
    const top = selected.offsetTop + (selected.offsetHeight - indicator.offsetHeight) / 2;
    indicator.style.transform = `translateY(${top}px)`;
    if (!indicator.dataset.placed) requestAnimationFrame(() => { indicator.dataset.placed = 'true'; });
  }
  /** New content in place of old, as in a sheet that changes page. */
  function swap(el) {
    animate(el, [{ opacity: 0.4, transform: 'translateX(10px)' }, { opacity: 1, transform: 'none' }], { duration: 200 }, 'swap');
  }
  /** Something changed state: a short pop draws the eye to it. */
  function pop(el) {
    animate(el, [{ transform: 'scale(.7)', opacity: 0.4 }, { transform: 'scale(1.08)', offset: 0.6 }, { transform: 'none', opacity: 1 }], { duration: 300 }, 'pop');
  }
  /** A number that changed rises into place. */
  function tick(el) {
    animate(el, [{ opacity: 0.3, transform: 'translateY(0.35em)' }, { opacity: 1, transform: 'none' }], { duration: 260 }, 'tick');
  }
  /** Results that just arrived settle in reading order, never hidden while they do. */
  function arrive(elements) {
    [...elements].slice(0, 24).forEach((el, i) =>
      animate(el, [{ opacity: 0.35, transform: 'translateY(10px)' }, { opacity: 1, transform: 'none' }], { duration: 260, delay: Math.min(i, 12) * 22, fill: 'backwards' }, 'arrive'));
  }
  /** A refusal or a second step: a short shake, the way a field says no. */
  function nudge(el) {
    animate(el, [{ transform: 'none' }, { transform: 'translateX(-4px)', offset: 0.25 }, { transform: 'translateX(3px)', offset: 0.55 }, { transform: 'translateX(-1px)', offset: 0.8 }, { transform: 'none' }], { duration: 320 }, 'nudge');
  }
  /** The cat answers: a hop for a success, a blink otherwise. */
  function cat(pose = 'blink') {
    for (const svg of document.querySelectorAll('svg.logo')) {
      if (!svg.getClientRects().length) continue;
      if (pose === 'success') animate(svg, [{ transform: 'none' }, { transform: 'translateY(-4px)', offset: 0.4 }, { transform: 'none' }], { duration: 300 }, 'cat');
      const tail = $('.cat-tail', svg), eyes = $('.cat-eyes', svg);
      if (tail && pose === 'success') animate(tail, [{ transform: 'none' }, { transform: 'translateX(-1px)', offset: 0.3 }, { transform: 'translateX(1px)', offset: 0.65 }, { transform: 'none' }], { duration: 320, easing: 'steps(2, end)' }, 'tail');
      if (eyes) animate(eyes, [{ transform: 'scaleY(1)' }, { transform: 'scaleY(.08)', offset: 0.35 }, { transform: 'scaleY(.08)', offset: 0.5 }, { transform: 'scaleY(1)' }], { duration: 230 }, 'blink');
    }
  }
  reduce.addEventListener('change', () => { if (reduce.matches) for (const a of document.getAnimations()) a.cancel(); });
  return { animate, enter, beforeLayout, afterLayout, progress, nav, swap, pop, tick, arrive, nudge, cat };
})();

/** A progress bar element; `Motion.progress` sets its value. */
export const progressBar = (labelText) =>
  h('div', { class: 'progress', role: 'progressbar', 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-label': labelText }, h('div', { class: 'progress-fill' }));
export function setProgress(bar, key, fraction) {
  bar.classList.remove('indeterminate');
  bar.setAttribute('aria-valuenow', Math.round(Math.min(1, Math.max(0, fraction)) * 100));
  Motion.progress(bar, key, fraction);
}
/** A bar for work under way whose amount is unknown: it moves, it never claims a value. */
export function setWorking(bar) {
  bar.classList.add('indeterminate');
  bar.removeAttribute('aria-valuenow');
}
