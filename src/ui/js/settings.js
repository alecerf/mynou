import { sentence } from './format.js';
import { $, h, icon } from './dom.js';
import { toast, act, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { data, views, itemsOf, currentParam } from './store.js';
import { INDEXER_ERRORS, plexError, indexerReports } from './sources.js';
import { signOut } from './session.js';

// The settings document keeps every field; the groups below edit the common
// ones in place and leave the others untouched. Saving sends the whole document,
// which the service validates and applies by restarting its engine.
// Settings come in pages, #settings/<page>, all in one form so a change on one
// page survives a visit to another until it is saved or discarded.
const SETTING_PAGES = [
  ['library', 'Library'], ['downloads', 'Downloads'], ['vpn', 'VPN'], ['indexers', 'Indexers'], ['services', 'Plex & TMDB'], ['network', 'Network'],
];
// A group is [page, title, description, fields]; a field is [path, label, type, options]: `hint` explains it, `when` names the
// switch it depends on, `scale` and `unit` show a stored number in friendlier
// units, and `zero` is what an empty field means.
const MB = 1_000_000;
const SETTING_GROUPS = [
  ['library', 'Library', 'Where finished films and series are filed.', [
    ['library.movies_root', 'Movies folder', 'text'],
    ['library.series_root', 'Series folder', 'text'],
  ]],
  ['downloads', 'Downloads', 'How the built-in client fetches releases.', [
    ['downloads.data_dir', 'Download folder', 'text'],
    ['downloads.max_active', 'Downloads at once', 'number'],
    ['downloads.max_peers', 'Peers per download', 'number', { hint: 'More peers, faster downloads. Up to 256, shared between active downloads.' }],
    ['downloads.download_limit_bps', 'Download speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
    ['downloads.upload_limit_bps', 'Upload speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
  ]],
  ['services', 'Plex', 'Your server and its watchlist.', [
    ['plex.url', 'Server address', 'text'],
    ['plex.token_env', 'Token', 'secret'],
    ['plex.movies_section', 'Movies section', 'text', { hint: 'The library number shown in Plex.' }],
    ['plex.series_section', 'Series section', 'text'],
    ['plex.watchlist', 'Watchlist', 'sync'],
  ]],
  ['services', 'TMDB', 'Titles, years and episode lists.', [
    ['catalog.token_env', 'Read access token', 'secret'],
  ]],
  ['library', 'Automation', 'What Mynou does on its own.', [
    ['monitoring.enabled', 'Look for better versions', 'check', { hint: 'Replaces a film or episode when a better release appears.' }],
    ['monitoring.interval_secs', 'Check every', 'number', { when: 'monitoring.enabled', scale: 60, unit: 'min' }],
    ['series_packs.enabled', 'Prefer season packs', 'check'],
    ['updates.automatic', 'Update Mynou automatically', 'check', { hint: 'Checked once a day.' }],
  ]],
];
const INDEXER_KINDS = ['torznab', 'json', 'rss'];
const settings = { report: null, document: null, secrets: [], access: null, loading: false };

const getPath = (object, path) => path.split('.').reduce((value, key) => value?.[key], object);
function setPath(object, path, value) {
  const keys = path.split('.');
  const last = keys.pop();
  let target = object;
  for (const key of keys) {
    if (typeof target[key] !== 'object' || target[key] === null) target[key] = {};
    target = target[key];
  }
  target[last] = value;
}
const settingValue = (path) => getPath(settings.document, path) ?? getPath(settings.report?.defaults, path);
const secretSource = (name) => settings.secrets.find((s) => s.name === name)?.source ?? 'missing';
const SOURCE_LABELS = { stored: 'Saved', environment: 'From the environment', missing: 'Not set' };
const SOURCE_TONES = { stored: 'ok', environment: 'ok', missing: 'warn' };
const SECRET_PROMPTS = { stored: 'Type to replace', environment: 'Type to override', missing: 'Paste it here' };

/** A secret is typed where it is used; the saved value is never shown again. */
function secretInput(name, label, placeholder, removable = true) {
  const source = secretSource(name);
  return h('span', { class: 'secret' },
    h('input', { type: 'password', autocomplete: 'new-password', spellcheck: false, maxLength: 4096, placeholder: placeholder ?? SECRET_PROMPTS[source], 'aria-label': label, dataset: { secret: name } }),
    source === 'stored' && removable ? h('button', { type: 'button', class: 'ghost danger', dataset: { forget: name } }, 'Remove') : null);
}

const shown = (value, type, scale = 1) => {
  if (value === undefined || value === null) return '';
  return type === 'number' ? String(Math.round((Number(value) / scale) * 100) / 100) : String(value);
};

function settingRow([path, label, type, options = {}]) {
  const value = settingValue(path);
  const id = `setting-${path.replaceAll('.', '-')}`;
  const source = type === 'secret' ? secretSource(String(value ?? '')) : null;
  const text = h('div', { class: 'setting-label' },
    h('label', { htmlFor: id }, label, source ? h('span', { class: 'badge', dataset: { tone: SOURCE_TONES[source] } }, SOURCE_LABELS[source]) : null),
    options.hint ? h('p', { class: 'hint' }, options.hint) : null);
  let control;
  if (type === 'sync') {
    return h('div', { class: 'setting-row toggle', id: 'plex-watchlist', hidden: !settingValue('plex.enabled') },
      h('div', { class: 'setting-label' }, h('span', {}, label), h('p', { class: 'hint', dataset: { live: 'watchlist' } })),
      h('button', { type: 'button', class: 'secondary', dataset: { act: 'sync' } }, icon('sync'), 'Sync now'));
  }
  if (type === 'check') control = h('input', { id, type: 'checkbox', class: 'switch', checked: Boolean(value), dataset: { path, type } });
  else if (type === 'secret') {
    control = secretInput(String(value ?? ''), label);
    $('input', control).id = id;
  } else {
    const display = options.zero && Number(value) === 0 ? '' : shown(value, type, options.scale);
    const input = h('input', {
      id, type: 'text', inputMode: type === 'number' ? (options.scale === MB ? 'decimal' : 'numeric') : undefined,
      value: display, placeholder: options.zero ?? '', spellcheck: false,
      dataset: { path, type, shown: display, scale: options.scale ?? 1, zero: options.zero ? '1' : '' },
    });
    control = options.unit ? h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, options.unit)) : input;
  }
  return h('div', { class: `setting-row${type === 'check' ? ' toggle' : ''}`, dataset: { when: options.when ?? '' } }, text, control);
}

/** Fields that depend on a switch only show while it is on. */
function syncDependents() {
  for (const row of $('#settings-form').querySelectorAll('[data-when]:not([data-when=""])')) {
    row.hidden = !$(`[data-path="${row.dataset.when}"]`)?.checked;
  }
}

function indexerRow(indexer) {
  const keyName = indexer.api_key_env ?? 'MYNOU_INDEXER_API_KEY';
  const row = h('li', { class: 'indexer' },
    h('div', { class: 'indexer-head' },
      h('input', { value: indexer.name ?? '', maxLength: 200, placeholder: 'Name', 'aria-label': 'Indexer name', dataset: { field: 'name' }, spellcheck: false }),
      h('select', { 'aria-label': 'Kind', dataset: { field: 'kind' } },
        INDEXER_KINDS.map((kind) => h('option', { value: kind, selected: (indexer.kind ?? 'json') === kind }, kind))),
      h('input', { type: 'checkbox', class: 'switch', checked: indexer.enabled !== false, 'aria-label': 'Enabled', title: 'Enabled', dataset: { field: 'enabled' } }),
      h('button', { type: 'button', class: 'icon-button danger', 'aria-label': 'Remove this indexer', dataset: { act: 'remove' } }, icon('close'))),
    h('div', { class: 'indexer-body' },
      h('input', { type: 'url', value: indexer.url ?? '', required: true, placeholder: 'Indexer address', 'aria-label': 'Indexer URL', spellcheck: false, dataset: { field: 'url' } }),
      // Indexers often share one key, so it is replaced here but never removed.
      secretInput(keyName, 'API key', `API key · ${SOURCE_LABELS[secretSource(keyName)].toLowerCase()}`, false)),
    h('p', { class: 'indexer-state', hidden: true, dataset: { live: 'indexer' } }));
  row._indexer = indexer;
  return row;
}

function readSettingsForm() {
  const document = structuredClone(settings.document);
  for (const input of $('#settings-form').querySelectorAll('[data-path]')) {
    const { path, type } = input.dataset;
    const before = getPath(document, path);
    let value;
    if (type === 'check') value = input.checked;
    else if (input.value.trim() === input.dataset.shown) continue; // untouched: keep the exact stored value
    else if (type === 'number') {
      const typed = input.value.trim().replace(',', '.');
      if (typed === '' && input.dataset.zero) value = 0;
      else {
        value = Math.round(Number(typed) * Number(input.dataset.scale));
        if (typed === '' || !Number.isFinite(value) || value < 0) throw new Error(`${$(`label[for="${input.id}"]`).textContent} needs a number`);
      }
    } else {
      value = input.value.trim();
      if (!value && before === undefined) continue;
    }
    // A field left at its default stays out of a document that omits it.
    if (before === undefined && value === getPath(settings.report?.defaults, path)) continue;
    setPath(document, path, value);
  }
  // Plex and TMDB have no switch: they run as soon as their credentials exist.
  const typed = typedSecrets();
  const present = (path) => {
    const name = String(settingValue(path) ?? '');
    return typed.has(name) || secretSource(name) !== 'missing';
  };
  for (const [path, on] of [['plex.enabled', present('plex.token_env')], ['catalog.enabled', present('catalog.token_env')]]) {
    if (on !== Boolean(settingValue(path))) setPath(document, path, on);
  }
  readNetwork(document);
  document.indexers = [...$('#settings-indexers').children].map((row) => {
    const indexer = { ...row._indexer };
    for (const input of row.querySelectorAll('[data-field]')) {
      indexer[input.dataset.field] = input.type === 'checkbox' ? input.checked : input.value.trim();
    }
    if (!indexer.name) delete indexer.name;
    return indexer;
  });
  return document;
}

/** Secrets typed in the form; empty inputs keep the saved value. */
function typedSecrets() {
  const typed = new Map();
  for (const input of $('#settings-form').querySelectorAll('[data-secret]')) {
    if (input.value) typed.set(input.dataset.secret, input.value);
  }
  return typed;
}

async function loadSettings() {
  // Network access is answered over HTTP only, never through the socket.
  const [report, secrets, access] = await Promise.all([link.call('GET', '/api/settings'), link.call('GET', '/api/secrets'), link.http('GET', '/api/access')]);
  settings.report = report;
  settings.document = report.settings;
  settings.secrets = itemsOf(secrets.secrets);
  settings.access = access;
  views.settings.paint();
}

// Network access: the switch and the port make `listen`, saved with the form.
// The password applies at once, and only on the Mac running Mynou.
const listenParts = (listen) => {
  const text = String(listen ?? '');
  const at = text.lastIndexOf(':');
  return { host: text.slice(0, at), port: text.slice(at + 1) };
};
const exposed = (listen) => !/^(127\.\d+\.\d+\.\d+|\[::1\])$/.test(listenParts(listen).host);

function networkRows() {
  const access = settings.access;
  const mac = access.from_this_mac;
  const listen = settingValue('listen');
  const on = exposed(listen);
  const { port } = listenParts(listen);
  const hosts = itemsOf(getPath(settings.document, 'access.allowed_hosts')).join(', ');
  const active = listenParts(access.listen).port;
  const where = access.local_name ?? access.address;
  const address = where ? `${location.protocol}//${where}${active === '80' ? '' : `:${active}`}` : null;
  const state = settings.report.restart_required ? 'Restart Mynou to apply the change.'
    : exposed(access.listen) ? (address ? `From another computer, open ${address}.` : 'Other computers can open Mynou.')
    : 'Only this Mac can open Mynou.';
  const row = (label, hint, control, toggle = false) => h('div', { class: `setting-row${toggle ? ' toggle' : ''}` },
    h('div', { class: 'setting-label' }, label, hint ? h('p', { class: 'hint' }, hint) : null), control);
  const badge = h('span', { class: 'badge', dataset: { tone: access.password_set ? 'ok' : 'warn' } }, access.password_set ? 'Set' : 'Not set');
  return [
    row(h('label', { htmlFor: 'network-on' }, 'Allow other computers'), state,
      h('input', { id: 'network-on', type: 'checkbox', class: 'switch', checked: on, disabled: !mac, dataset: { shown: String(on) } }), true),
    row(h('label', { htmlFor: 'network-port' }, 'Port'), 'With 80, addresses need no port.',
      h('input', { id: 'network-port', type: 'text', inputMode: 'numeric', value: port, disabled: !mac, spellcheck: false, dataset: { shown: port } })),
    row(h('label', { htmlFor: 'network-hosts' }, 'Other host names'), `Mynou already answers this Mac's addresses${access.local_name ? ` and ${access.local_name}` : ''}. Add other names, separated by commas.`,
      h('input', { id: 'network-hosts', type: 'text', value: hosts, disabled: !mac, spellcheck: false, placeholder: 'None', dataset: { shown: hosts } })),
    row(h('label', { htmlFor: 'network-password' }, 'Password', badge), 'At least 10 characters. A new password signs every other computer out.',
      h('span', { class: 'secret' },
        h('input', { id: 'network-password', type: 'password', autocomplete: 'new-password', maxLength: 1024, disabled: !mac, placeholder: access.password_set ? 'Type to replace' : 'Choose a password' }),
        h('button', { type: 'button', class: 'secondary', disabled: !mac, dataset: { networkAct: 'password' } }, 'Save password'),
        access.password_set && !on && mac ? h('button', { type: 'button', class: 'ghost danger', dataset: { networkAct: 'remove' } }, 'Remove') : null)),
    mac ? null : row(h('span', {}, 'This computer'), 'Network access changes only on the Mac running Mynou.',
      h('button', { type: 'button', class: 'ghost', dataset: { networkAct: 'sign-out' } }, 'Sign out')),
  ].filter(Boolean);
}

/** Folds the switch, port and host names into the document, when changed. */
function readNetwork(document) {
  const on = $('#network-on');
  if (!on || on.disabled) return;
  const port = $('#network-port').value.trim();
  if (String(on.checked) !== on.dataset.shown || port !== $('#network-port').dataset.shown) {
    if (!/^\d{1,5}$/.test(port) || Number(port) < 1 || Number(port) > 65535) throw new Error('Port needs a number from 1 to 65535');
    if (on.checked && !settings.access.password_set) throw new Error('Save a password first, then allow other computers');
    document.listen = `${on.checked ? '0.0.0.0' : '127.0.0.1'}:${Number(port)}`;
  }
  const hosts = $('#network-hosts');
  if (hosts.value.trim() !== hosts.dataset.shown) {
    setPath(document, 'access.allowed_hosts', hosts.value.split(/[\s,]+/).filter(Boolean));
  }
}

async function networkAction(button) {
  const action = button.dataset.networkAct;
  if (action === 'sign-out') { await signOut(); return; }
  let password = null;
  if (action === 'password') {
    password = $('#network-password').value;
    if ([...password].length < 10) throw new Error('The password needs at least 10 characters');
  } else if (!confirm('Remove the network access password?')) return;
  settings.access = await link.http('POST', '/api/access', { password });
  $('#settings-network').replaceChildren(...networkRows());
  toast(password === null ? 'Password removed' : 'Password saved');
}

const settingsState = (text) => { const state = $('#settings-state'); state.textContent = text; state.hidden = !text; };

async function saveSettings(document) {
  const reply = await link.call('POST', '/api/settings', { settings: document });
  if (reply.state === 'applying') {
    settingsState('Applying the new settings…');
    // The service restarts its engine; wait until the change has landed.
    for (let i = 0; i < 60; i++) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
      const report = await link.call('GET', '/api/settings').catch(() => null);
      if (report && !report.applying) {
        if (report.error) throw new Error(report.error);
        break;
      }
    }
  }
  await loadSettings();
  toast(reply.restart_required ? 'Settings saved; restart Mynou to apply them' : 'Settings applied');
}

// VPN: its controls apply at once, outside the settings document and Save.
const VPN_STATES = {
  protected: ['Protected', 'ok'], connecting: ['Connecting', 'warn'], reconnecting: ['Reconnecting', 'warn'],
  disabled: ['Off', 'warn'], blocked: ['Blocked', 'bad'], error: ['Error', 'bad'], unchosen: ['Not chosen', 'warn'],
};
const VPN_DONE = { enable: 'VPN turned on: connecting', disable: 'Torrents use your own connection' };
const VPN_REASONS = {
  GaveUp: 'The VPN server never answered. Check the profile, or try another server.',
  Expired: 'The session expired.', Failed: 'The tunnel failed.', EndpointChanged: 'The server address changed.',
  Cancelled: 'Stopped.', Exhausted: 'Every reconnection failed.',
};
const VPN_RESTARTING = 'Retiring previous engine generation';
const VPN_REVOKED = 'Profile revoked by operator';
const vpnControl = (body) => link.call('POST', '/api/vpn/control', body);

function vpnRows() {
  const label = (text) => h('span', {}, text, h('span', { class: 'badge' }));
  return [
    h('div', { class: 'setting-row toggle', dataset: { vpn: 'state' } },
      h('div', { class: 'setting-label' }, label('Tunnel'), h('p', { class: 'hint' })),
      h('div', { class: 'buttons' },
        h('button', { type: 'button', class: 'secondary', dataset: { vpnAct: 'enable' } }, 'Turn on'),
        h('button', { type: 'button', class: 'ghost danger', dataset: { vpnAct: 'disable' } }, 'Turn off'))),
    h('div', { class: 'setting-row toggle', dataset: { vpn: 'profile' } },
      h('div', { class: 'setting-label' }, label('WireGuard profile'), h('p', { class: 'hint' }, 'The WireGuard .conf file from your provider.')),
      h('button', { type: 'button', class: 'secondary', dataset: { vpnAct: 'import' } }),
      h('input', { type: 'file', accept: '.conf', hidden: true, dataset: { vpn: 'file' } })),
    h('div', { class: 'setting-row toggle', dataset: { vpn: 'forwarding' } },
      h('div', { class: 'setting-label' }, h('label', { htmlFor: 'vpn-forwarding' }, 'Open a port for seeding'), h('p', { class: 'hint' })),
      h('input', { id: 'vpn-forwarding', type: 'checkbox', class: 'switch' })),
  ];
}

/** The tunnel as the last status push reports it. */
function paintVpn() {
  const list = $('#settings-vpn');
  if (!list.childElementCount) list.replaceChildren(...vpnRows());
  const vpn = data.get('status')?.data?.vpn;
  const row = (name) => $(`[data-vpn="${name}"]`, list);
  const external = vpn?.state === 'external';
  const profile = Boolean(vpn?.profile_present);
  const revoked = vpn?.reason === VPN_REVOKED;
  const [state, tone] = !vpn ? ['…', 'warn'] : external ? ['Not managed here', 'warn']
    : vpn.reason === VPN_RESTARTING ? ['Restarting', 'warn'] : VPN_STATES[vpn.state] ?? [sentence(vpn.state), 'warn'];
  const badge = $('.badge', row('state'));
  badge.textContent = state;
  badge.dataset.tone = tone;
  const age = Number(vpn?.handshake_age_secs);
  $('.hint', row('state')).textContent = external ? 'This Mynou uses an outside network; it runs no VPN of its own.'
    : vpn?.state === 'protected' ? `${vpn.ipv6 ? 'IPv4 and IPv6.' : 'IPv4 only.'} ${Number.isFinite(age) && vpn.handshake_age_secs !== null ? `Last handshake ${age} s ago.` : 'Connected.'}`
    : vpn?.state === 'disabled' ? (profile ? 'Torrents use your own connection.' : 'Import a profile to protect your torrents.')
    : vpn?.state === 'connecting' ? 'Waiting for the VPN server to answer.'
    : vpn?.state === 'unchosen' ? 'Torrents wait until you choose: import a WireGuard profile, or use your own connection.'
    : vpn?.reason && vpn.reason !== VPN_RESTARTING ? VPN_REASONS[vpn.reason] ?? vpn.reason : '';
  // A failed tunnel is retried from here: turning it off would send torrents direct.
  const failed = vpn?.enabled && !revoked && (vpn.state === 'error' || (vpn.state === 'blocked' && vpn.reason !== VPN_RESTARTING));
  const enable = $('[data-vpn-act="enable"]', list);
  enable.hidden = external || !profile || (vpn.enabled && !revoked && !failed);
  enable.textContent = failed ? 'Try again' : 'Turn on';
  const unchosen = vpn?.state === 'unchosen';
  const disable = $('[data-vpn-act="disable"]', list);
  disable.hidden = external || !(vpn?.enabled || unchosen);
  disable.textContent = unchosen ? 'Use my own connection' : 'Turn off';
  row('profile').hidden = !vpn || external;
  const imported = $('.badge', row('profile'));
  imported.textContent = profile ? 'Imported' : 'Not imported';
  imported.dataset.tone = profile ? 'ok' : 'warn';
  $('[data-vpn-act="import"]', list).textContent = profile ? 'Replace' : 'Import a .conf';
  row('forwarding').hidden = !profile || external;
  const forwarding = $('#vpn-forwarding');
  if (!forwarding.disabled) forwarding.checked = Boolean(vpn?.forwarding) && vpn.forwarding !== 'disabled';
  const hint = $('.hint', row('forwarding'));
  hint.textContent = {
    active: `Port ${vpn?.forwarded_port} is open to other peers.`,
    acquiring: 'Asking the VPN server for a port…',
    unavailable: `Unavailable: ${vpn?.forwarding_reason ?? 'the server refused'}.`,
  }[vpn?.forwarding] ?? 'Needs a server with port forwarding (NAT-PMP), such as Proton P2P servers.';
  hint.classList.toggle('bad', vpn?.forwarding === 'unavailable');
}

/** Shows the groups of the page in the hash; the first page by default. */
function showPage() {
  const wanted = currentParam();
  const page = SETTING_PAGES.some(([id]) => id === wanted) ? wanted : SETTING_PAGES[0][0];
  for (const link of $('#settings-nav').children) {
    if (link.dataset.page === page) link.setAttribute('aria-current', 'page'); else link.removeAttribute('aria-current');
  }
  let first = true;
  for (const group of $('#settings-form').querySelectorAll('.setting-group[data-page]')) {
    group.hidden = group.dataset.page !== page;
    group.classList.toggle('first', !group.hidden && first);
    if (!group.hidden) first = false;
  }
}

async function saveSecret(name, value) {
  const reply = await link.call('POST', '/api/secrets', { name, value });
  settings.secrets = itemsOf(reply.secrets);
}

views.settings = {
  init() {
    $('#settings-nav').replaceChildren(...SETTING_PAGES.map(([page, title]) =>
      h('a', { href: `#settings/${page}`, dataset: { page } }, title)));
    const form = $('#settings-form');
    const dirty = () => {
      const bar = $('#save-bar');
      if (!bar.hidden) return;
      bar.hidden = false;
      Motion.animate(bar, [{ opacity: 0, transform: 'translateY(16px) scale(.97)' }, { opacity: 1, transform: 'none' }], { duration: 240 });
    };
    form.addEventListener('input', dirty);
    form.addEventListener('change', (event) => { if (event.target.dataset.path) syncDependents(); });
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      act($('button[type="submit"]', form), async () => {
        const document = readSettingsForm();
        for (const [name, value] of typedSecrets()) await saveSecret(name, value);
        await saveSettings(document);
      });
    });
    $('#settings-discard').addEventListener('click', () => this.paint());
    const network = $('#settings-network');
    // The password applies at once: typing it never marks the settings unsaved.
    network.addEventListener('input', (event) => { if (event.target.id === 'network-password') event.stopPropagation(); });
    network.addEventListener('click', (event) => {
      const button = event.target.closest('[data-network-act]');
      if (button) act(button, () => networkAction(button));
    });
    const vpn = $('#settings-vpn');
    // VPN controls apply at once: they never mark the settings unsaved.
    vpn.addEventListener('input', (event) => event.stopPropagation());
    vpn.addEventListener('click', (event) => {
      const button = event.target.closest('[data-vpn-act]');
      if (!button) return;
      const action = button.dataset.vpnAct;
      if (action === 'import') { $('[data-vpn="file"]', vpn).click(); return; }
      if (action === 'disable' && !confirm(data.get('status')?.data?.vpn?.state === 'unchosen'
        ? 'Use your own connection? Peers and trackers will see your address.'
        : 'Turn off the VPN? Torrents will then use your own connection.')) return;
      act(button, () => vpnControl({ action }), VPN_DONE[action]);
    });
    vpn.addEventListener('change', (event) => {
      event.stopPropagation();
      const input = event.target;
      if (input.dataset.vpn === 'file') {
        const file = input.files[0];
        input.value = '';
        if (!file) return;
        act($('[data-vpn-act="import"]', vpn), async () => {
          if (file.size > 16_384) throw new Error('This is not a WireGuard profile: it is larger than 16 KB');
          await vpnControl({ action: 'import', profile: await file.text() });
        }, 'Profile imported: connecting');
      } else if (input.id === 'vpn-forwarding') {
        const on = input.checked;
        input.disabled = true;
        vpnControl({ action: on ? 'enable-forwarding' : 'disable-forwarding' })
          .then(() => toast(on ? 'Asking the VPN server for a port' : 'Port closed'))
          .catch((e) => { input.checked = !on; toast(e.message, 'bad'); })
          .finally(() => { input.disabled = false; });
      }
    });
    $('#indexer-add').addEventListener('click', () => {
      const row = indexerRow({ kind: 'torznab', enabled: true });
      $('#settings-indexers').append(row);
      Motion.animate(row, [{ opacity: 0, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
      $('input', row).focus();
      dirty();
    });
    form.addEventListener('click', (event) => {
      const remove = event.target.closest('#settings-indexers [data-act="remove"]');
      if (remove) { remove.closest('li').remove(); dirty(); return; }
      const sync = event.target.closest('[data-act="sync"]');
      if (sync) {
        act(sync, async () => { const r = await link.call('POST', '/api/sync', {}); toast(r.submitted ? `${r.submitted} new ${r.submitted === 1 ? 'request' : 'requests'} from the watchlist` : 'Watchlist read; nothing new'); });
        return;
      }
      const forget = event.target.closest('[data-forget]');
      if (forget) act(forget, async () => { await saveSecret(forget.dataset.forget, null); this.paint(); }, 'Secret removed');
    });
  },
  render() {
    showPage();
    // Live updates never overwrite fields being edited.
    if (!settings.report && !$('#settings-groups').childElementCount) {
      $('#settings-form').setAttribute('aria-busy', 'true');
      $('#settings-groups').replaceChildren(...Array.from({ length: 3 }, () => h('section', { class: 'setting-group' },
        h('div', { class: 'setting-intro' }, skeleton('lines', 2)), h('ol', { class: 'setting-list' }, skeleton('rows', 3)))));
    }
    if (!settings.report && link.state === 'open' && !settings.loading) {
      settings.loading = true;
      loadSettings().catch((e) => { settingsState(e.message); }).finally(() => { settings.loading = false; });
    }
    if (settings.report) this.paintLive();
  },
  paint() {
    const report = settings.report;
    $('#settings-form').removeAttribute('aria-busy');
    settingsState([report.applying && 'A change is being applied.', report.restart_required && 'The listen address changes at the next start.'].filter(Boolean).join(' '));
    if (report.error) toast(report.error, 'bad');
    $('#settings-groups').replaceChildren(...SETTING_GROUPS.map(([page, title, about, fields]) =>
      h('section', { class: 'setting-group', dataset: { page } },
        h('div', { class: 'setting-intro' }, h('h2', {}, title), h('p', { class: 'hint' }, about)),
        h('div', { class: 'setting-list' }, fields.map(settingRow)))));
    // Saved indexers keep their position, which is also the order of the live reports.
    $('#settings-indexers').replaceChildren(...itemsOf(settings.document.indexers).map((indexer, i) => Object.assign(indexerRow(indexer), { _saved: i })));
    $('#settings-network').replaceChildren(...networkRows());
    $('#save-bar').hidden = true;
    syncDependents();
    showPage();
    this.paintLive();
  },
  /** States that change while the page is open; inputs are never touched. */
  paintLive() {
    paintVpn();
    const reports = indexerReports();
    for (const row of $('#settings-indexers').children) {
      const state = $('[data-live="indexer"]', row);
      const report = row._saved === undefined ? null : reports[row._saved];
      const searches = Number(report?.successes) || 0;
      let text = '';
      let tone = 'ok';
      if (report?.last_error && report.last_error !== 'disabled') [text, tone] = [INDEXER_ERRORS[report.last_error] ?? sentence(report.last_error), 'bad'];
      else if (report?.enabled === false) [text, tone] = ['Off', 'warn'];
      else if (searches) text = `Working · ${searches} ${searches === 1 ? 'search' : 'searches'} answered`;
      else if (report) [text, tone] = ['Not used yet', 'warn'];
      state.hidden = !text;
      state.textContent = text;
      state.dataset.tone = tone;
    }
    const status = data.get('status')?.data;
    const hint = $('[data-live="watchlist"]');
    if (!hint || !status) return;
    const accounts = data.get('requesters')?.data?.accounts?.length ?? 0;
    hint.textContent = status.last_sync_error ? `Last sync failed: ${plexError(status.last_sync_error)}`
      : `${accounts ? `${accounts} accounts' watchlists` : 'Your watchlist'} is read every minute.`;
    hint.classList.toggle('bad', Boolean(status.last_sync_error));
  },
};
