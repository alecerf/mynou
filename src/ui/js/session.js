import { $ } from './dom.js';
import { toast, busy } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';

// The access token: asked for, created on a first start, kept for the session.

export const TOKEN_KEY = 'mynou.token';

export function askToken(message = '') {
  const dialog = $('#login');
  $('#login-error').textContent = message;
  if (!dialog.open) dialog.showModal();
  if (message) Motion.nudge($('form', dialog));
  $('input', dialog).focus();
}

/** The first visit of a new installation creates its access token here. */
export function firstStart() {
  const dialog = $('#first-start');
  dialog.addEventListener('cancel', (event) => event.preventDefault());
  $('#first-start-button').addEventListener('click', async (event) => {
    $('#first-start-error').textContent = '';
    const button = event.currentTarget;
    button.disabled = true;
    const idle = busy(button);
    try {
      const response = await fetch('/api/setup', { method: 'POST', headers: { 'Content-Type': 'application/json' }, signal: AbortSignal.timeout(10000) });
      const body = await response.json();
      if (!response.ok || !body.token) throw new Error(body.error ?? 'The access token could not be created');
      $('#first-start-token').value = body.token;
      $('#first-start-create').hidden = true;
      $('#first-start-created').hidden = false;
      $('#first-start-token').select();
    } catch (error) {
      $('#first-start-error').textContent = error.message;
      Motion.nudge($('#first-start-form'));
    } finally {
      idle();
      button.disabled = false;
    }
  });
  $('#first-start-saved').addEventListener('change', (event) => { $('#first-start-continue').disabled = !event.currentTarget.checked; });
  $('#first-start-copy').addEventListener('click', async () => {
    const button = $('#first-start-copy');
    try {
      await navigator.clipboard.writeText($('#first-start-token').value);
      toast('Access token copied');
      button.textContent = 'Copied';
      setTimeout(() => { button.textContent = 'Copy'; }, 2000);
    } catch { $('#first-start-token').select(); }
  });
  $('#first-start-form').addEventListener('submit', () => {
    const token = $('#first-start-token').value;
    $('#first-start-token').value = '';
    try { sessionStorage.setItem(TOKEN_KEY, token); } catch { /* storage unavailable: the token lives for this page only */ }
    link.connect(token);
  });
}

/** Asks for the token, unless this installation has none yet. */
export async function connectOrSetUp() {
  for (;;) {
    try {
      const response = await fetch('/api/setup', { signal: AbortSignal.timeout(5000) });
      if (response.status === 503) { await new Promise((resolve) => setTimeout(resolve, 1000)); continue; }
      const report = await response.json();
      if (report.available) { $('#first-start').showModal(); return; }
      askToken(report.token_set ? '' : 'This installation has no access token yet: run `mynou token --reset` on the machine running Mynou while it is stopped, then paste the token here.');
      return;
    } catch {
      askToken();
      return;
    }
  }
}
