// Light, dark or the system's choice, remembered by this browser only.

const THEME_KEY = 'mynou.theme';
export const THEMES = { auto: 'Match the system', light: 'Light', dark: 'Dark' };

/** Auto leaves the colours to the system; light or dark pins them for this browser. */
function applyTheme(choice) {
  const theme = Object.hasOwn(THEMES, choice) ? choice : 'auto';
  const root = document.documentElement;
  if (theme === 'auto') delete root.dataset.theme; else root.dataset.theme = theme;
  document.querySelector('meta[name="color-scheme"]').content = theme === 'auto' ? 'dark light' : theme;
  for (const button of document.querySelectorAll('#theme [data-choice]')) button.setAttribute('aria-pressed', String(button.dataset.choice === theme));
  return theme;
}

function storeTheme(choice) {
  const theme = applyTheme(choice);
  try { if (theme === 'auto') localStorage.removeItem(THEME_KEY); else localStorage.setItem(THEME_KEY, theme); } catch { /* storage unavailable: the choice lasts for this page */ }
}

/** Applies a theme; the new colours spread in a circle from `origin`, a
    point on the screen (its centre by default), where the browser can. */
export function setTheme(choice, origin) {
  const before = document.documentElement.dataset.theme ?? 'auto';
  const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (still || before === choice || !document.startViewTransition || !('types' in ViewTransition.prototype)) { storeTheme(choice); return; }
  const x = origin?.x ?? innerWidth / 2, y = origin?.y ?? innerHeight / 2;
  const radius = Math.hypot(Math.max(x, innerWidth - x), Math.max(y, innerHeight - y));
  const shift = document.startViewTransition({ update: () => storeTheme(choice), types: ['theme'] });
  shift.ready.then(() => document.documentElement.animate(
    { clipPath: [`circle(0px at ${x}px ${y}px)`, `circle(${radius}px at ${x}px ${y}px)`] },
    { duration: 700, easing: 'cubic-bezier(.22,1,.36,1)', pseudoElement: '::view-transition-new(root)' },
  )).catch(() => { /* skipped: the theme is applied all the same */ });
}

// Before anything else draws, so a pinned theme does not flash the system one.
try { applyTheme(localStorage.getItem(THEME_KEY)); } catch { /* storage unavailable: follow the system */ }
// Another tab changed it.
addEventListener('storage', (event) => { if (event.key === THEME_KEY) applyTheme(event.newValue); });
