import { $ } from './dom.js';
import { busy } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';

// Signing in from another computer on the network. The Mac running Mynou never
// signs in; elsewhere the password buys an HttpOnly session cookie.

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function request(method, body) {
  const response = await fetch('/api/session', {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(15000),
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw Object.assign(new Error(data.error ?? `Request failed (${response.status})`), { status: response.status });
  return data;
}

function ask(message = '') {
  const dialog = $('#sign-in');
  $('#sign-in-error').textContent = message;
  if (!dialog.open) dialog.showModal();
  if (message) Motion.nudge($('form', dialog));
  $('input', dialog).focus();
}

/** Whether this browser may use the API now; asks for the password if not. */
async function admitted() {
  for (;;) {
    try {
      const state = await request('GET');
      if (!state.required || state.signed_in) return true;
      ask(state.password_set ? '' : 'Network access has no password yet. Set one in Settings, Network, on the Mac running Mynou.');
      return false;
    } catch { await sleep(1000); /* starting or unreachable: try again */ }
  }
}

export async function connect() {
  if (await admitted()) link.connect();
}

export async function signOut() {
  await request('DELETE');
  link.disconnect();
  ask();
}

export function setupSession() {
  const dialog = $('#sign-in');
  dialog.addEventListener('cancel', (event) => event.preventDefault());
  $('#sign-in-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const password = new FormData(form).get('password');
    $('#sign-in-error').textContent = '';
    const idle = busy($('button', form));
    try {
      await request('POST', { password });
      form.reset();
      dialog.close();
      link.connect();
    } catch (error) {
      $('#sign-in-error').textContent = error.message;
      Motion.nudge(form);
    } finally { idle(); }
  });
  // A session that ended (expired, or a new password) closes the socket.
  let checking = false;
  link.addEventListener('state', async ({ detail }) => {
    if (detail !== 'closed' || checking) return;
    checking = true;
    try {
      const state = await request('GET');
      if (state.required && !state.signed_in) {
        link.disconnect();
        ask('Your session ended. Sign in again.');
      }
    } catch { /* unreachable: the link keeps retrying */ } finally { checking = false; }
  });
}
