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

export function setTheme(choice) {
  const theme = applyTheme(choice);
  try { if (theme === 'auto') localStorage.removeItem(THEME_KEY); else localStorage.setItem(THEME_KEY, theme); } catch { /* storage unavailable: the choice lasts for this page */ }
}

// Before anything else draws, so a pinned theme does not flash the system one.
try { applyTheme(localStorage.getItem(THEME_KEY)); } catch { /* storage unavailable: follow the system */ }
// Another tab changed it.
addEventListener('storage', (event) => { if (event.key === THEME_KEY) applyTheme(event.newValue); });
