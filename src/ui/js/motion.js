import { $, h } from './dom.js';

// Every DOM change happens first; an animation is a cancellable response to
// it, never a condition for an action. The browser's reduced-motion setting
// turns all of it off.
export const Motion = (() => {
  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  const channels = new WeakMap();
  const progressValues = new Map();
  const ease = 'cubic-bezier(.22,1,.36,1)';
  // The springs of the stylesheet, for animations started from here.
  const token = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const springs = CSS.supports('transition-timing-function', 'linear(0, 1)');
  const spring = () => (springs && token('--spring')) || ease;
  const bouncy = () => (springs && token('--bouncy')) || ease;
  function animate(el, frames, options = {}, channel = 'response') {
    if (!el?.animate || reduce.matches) return null;
    let owned = channels.get(el);
    if (!owned) { owned = new Map(); channels.set(el, owned); }
    owned.get(channel)?.cancel();
    const animation = el.animate(frames, { duration: 200, easing: ease, ...options });
    owned.set(channel, animation);
    return animation;
  }
  /** A page that arrives rises into place, or comes down when `back`. */
  const enter = (view, back = false) =>
    animate(view, [{ opacity: 0, transform: `translateY(${back ? -20 : 20}px)` }, { opacity: 1, transform: 'none' }], { duration: 560 }, 'view');
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
      else animate(el, [{ transform: `translate(${x}px, ${y}px)` }, { transform: 'none' }], { duration: 480, easing: spring() }, 'layout');
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
  /** The pill under the current link of the sidebar, the foot's included;
      it slides once it has a first place. */
  function nav() {
    const indicator = $('#nav-indicator');
    if (!indicator) return;
    const selected = $('.sidebar .nav-link[aria-current="page"]');
    if (!selected?.offsetParent) { indicator.dataset.state = 'off'; delete indicator.dataset.placed; return; }
    indicator.dataset.state = 'on';
    indicator.style.width = `${selected.offsetWidth}px`;
    indicator.style.height = `${selected.offsetHeight}px`;
    indicator.style.transform = `translate(${selected.offsetLeft}px, ${selected.offsetTop}px)`;
    if (!indicator.dataset.placed) requestAnimationFrame(() => { indicator.dataset.placed = 'true'; });
  }
  const counts = new WeakMap();
  /** A figure shown for the first time counts up to its value, then reads `text`. */
  function count(el, to, text) {
    cancelAnimationFrame(counts.get(el));
    if (reduce.matches || !Number.isFinite(to) || to <= 0) { el.textContent = text; return; }
    const start = performance.now(), duration = 750;
    const step = (now) => {
      const t = Math.min(1, (now - start) / duration);
      el.textContent = t < 1 ? Math.round(to * (1 - (1 - t) ** 4)).toLocaleString() : text;
      if (t < 1) counts.set(el, requestAnimationFrame(step));
    };
    el.textContent = '0';
    counts.set(el, requestAnimationFrame(step));
  }
  /** Stops a count under way and shows `text` at once. */
  function settle(el, text) {
    cancelAnimationFrame(counts.get(el));
    el.textContent = text;
  }
  /** New content in place of old, as in a sheet that changes page. */
  function swap(el) {
    animate(el, [{ opacity: 0.4, transform: 'translateX(14px)' }, { opacity: 1, transform: 'none' }], { duration: 420, easing: spring() }, 'swap');
  }
  /** Something changed state: a short pop draws the eye to it. */
  function pop(el) {
    animate(el, [{ transform: 'scale(.6)', opacity: 0.4 }, { transform: 'none', opacity: 1 }], { duration: 550, easing: bouncy() }, 'pop');
  }
  /** A number that changed rises into place. */
  function tick(el) {
    animate(el, [{ opacity: 0.3, transform: 'translateY(0.45em)' }, { opacity: 1, transform: 'none' }], { duration: 450, easing: spring() }, 'tick');
  }
  /** Results that just arrived settle in reading order, never hidden while they do. */
  function arrive(elements) {
    [...elements].slice(0, 24).forEach((el, i) =>
      animate(el, [{ opacity: 0, transform: 'translateY(16px) scale(.97)' }, { opacity: 1, transform: 'none' }], { duration: 520, easing: spring(), delay: Math.min(i, 12) * 28, fill: 'backwards' }, 'arrive'));
  }
  /** A refusal or a second step: a short shake, the way a field says no. */
  function nudge(el) {
    animate(el, [{ transform: 'none' }, { transform: 'translateX(-4px)', offset: 0.25 }, { transform: 'translateX(3px)', offset: 0.55 }, { transform: 'translateX(-1px)', offset: 0.8 }, { transform: 'none' }], { duration: 320 }, 'nudge');
  }
  /** `el` drifts up and out of focus over the first `rems` of scrolling. Set
      from here rather than by a scroll timeline: Safari now and then left the
      blur on once the page was back at the top. */
  function fadeOnScroll(el, rems) {
    let queued = false;
    const apply = () => {
      queued = false;
      const rem = parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
      const t = reduce.matches || !el.offsetParent ? 0 : Math.min(1, Math.max(0, scrollY / (rems * rem)));
      if (t === 0) { el.style.removeProperty('opacity'); el.style.removeProperty('translate'); el.style.removeProperty('filter'); return; }
      el.style.opacity = String(1 - t);
      el.style.translate = `0 ${(-2.5 * t).toFixed(3)}rem`;
      el.style.filter = `blur(${(10 * t).toFixed(2)}px)`;
    };
    addEventListener('scroll', () => { if (!queued) { queued = true; requestAnimationFrame(apply); } }, { passive: true });
    apply();
  }
  reduce.addEventListener('change', () => { if (reduce.matches) for (const a of document.getAnimations()) a.cancel(); });
  return { animate, enter, spring, bouncy, beforeLayout, afterLayout, progress, nav, count, settle, swap, pop, tick, arrive, nudge, fadeOnScroll };
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
