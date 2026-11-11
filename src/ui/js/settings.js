import { sentence, relativeTime } from './format.js';
import { $, h, icon } from './dom.js';
import { toast, act, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { data, views, itemsOf, currentParam } from './store.js';
import { INDEXER_ERRORS, plexError, indexerReports } from './sources.js';
import { signOut } from './session.js';
import { qualityGroups, readQuality, forgetProfiles } from './quality.js';

// The settings document keeps every field; the groups below edit the common
// ones in place and leave the others untouched. Saving sends the whole document,
// which the service validates and applies by restarting its engine.
// Settings come in pages, #settings/<page>, all in one form so a change on one
// page survives a visit to another until it is saved or discarded.
const SETTING_PAGES = [
  ['library', 'Library'], ['downloads', 'Downloads'], ['quality', 'Quality'], ['vpn', 'VPN'], ['indexers', 'Indexers'], ['services', 'Plex & TMDB'], ['network', 'Network'],
];
// A group is [page, title, description, fields]; a field is [path, label, type, options]: `hint` explains it, `when` names the
// switch it depends on, `scale` and `unit` show a stored number in friendlier
// units, and `zero` is what an empty field means.
const MB = 1_000_000;
const SETTING_GROUPS = [
  ['library', 'Library', 'Where finished films and series are filed.', [
    ['library.movies_root', 'Movies folder', 'text', { folder: true, hint: 'A full path on the Mac running Mynou.' }],
    ['library.series_root', 'Series folder', 'text', { folder: true, hint: 'A full path on the Mac running Mynou.' }],
  ]],
  ['downloads', 'Downloads', 'How the built-in client fetches releases.', [
    ['downloads.enabled', 'Download torrents', 'check', { hint: 'Off: nothing downloads or seeds, and requests wait. Titles already in Plex are still found, for example while you move an existing library.' }],
    ['downloads.data_dir', 'Download folder', 'text', { folder: true, hint: 'A full path on the Mac running Mynou.' }],
    ['downloads.max_active', 'Downloads at once', 'number', { zero: 'No limit', hint: 'Empty for no limit: every download runs at once.' }],
    ['downloads.max_peers', 'Peers per download', 'number', { zero: 'No limit', hint: 'More peers, faster downloads. Empty for no limit: as many as the swarm offers and memory allows.' }],
    ['downloads.download_limit_bps', 'Download speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
    ['downloads.upload_limit_bps', 'Upload speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
    ['downloads.cross_seed_missing_bytes', 'Cross-seed extras', 'number', { scale: MB, unit: 'MB', zero: 'None', hint: 'A cross-seed may download the small files it lacks, such as an .nfo or a sample, up to this much. Video is never downloaded.' }],
  ]],
  ['services', 'Plex', 'Your server and its watchlist.', [
    ['plex.server', 'Server', 'plex'],
    ['plex.movies_section', 'Films go to', 'section', { kind: 'movie' }],
    ['plex.series_section', 'Series go to', 'section', { kind: 'show' }],
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
// What goes through the VPN while it is on; saved with the other settings.
const VPN_USES = [
  ['vpn.torrents', 'Torrents', 'check', { hint: 'Peers and trackers see the VPN’s address instead of yours.' }],
  ['vpn.indexers', 'Indexers', 'check', { hint: 'Searches and feeds, IRC announces included.' }],
  ['vpn.tmdb', 'TMDB', 'check', { hint: 'Titles, posters and episode lists.' }],
];
const INDEXER_KINDS = ['torznab', 'json', 'rss'];
const settings = { report: null, document: null, secrets: [], access: null, loading: false, plexSections: null };

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

// Plex connects in one click: Plex's own page signs in, then a server is chosen.
const plexConnected = () => Boolean(settingValue('plex.enabled')) && secretSource(String(settingValue('plex.token_env') ?? '')) !== 'missing';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function plexRows() {
  const connected = plexConnected();
  const account = h('div', { class: 'setting-row toggle', id: 'plex-account' },
    h('div', { class: 'setting-label' },
      h('span', {}, connected ? 'Server' : 'Account', h('span', { class: 'badge', dataset: { tone: connected ? 'ok' : 'warn' } }, connected ? 'Connected' : 'Not connected')),
      h('p', { class: 'hint' }, connected ? String(settingValue('plex.url') ?? '') : 'Sign in with Plex, then Mynou finds your server and its libraries.')),
    h('span', { class: 'secret' }, connected
      ? [h('button', { type: 'button', class: 'secondary', dataset: { plexAct: 'servers' } }, 'Change server'),
        h('button', { type: 'button', class: 'ghost danger', dataset: { plexAct: 'disconnect' } }, 'Disconnect')]
      // Signed in without a server yet: only the choice remains.
      : secretSource(String(settingValue('plex.token_env') ?? '')) === 'missing'
        ? h('button', { type: 'button', dataset: { plexAct: 'connect' } }, 'Connect to Plex')
        : [h('button', { type: 'button', dataset: { plexAct: 'servers' } }, 'Choose a server'),
          h('button', { type: 'button', class: 'ghost danger', dataset: { plexAct: 'disconnect' } }, 'Disconnect')]));
  const servers = h('div', { class: 'setting-row', id: 'plex-servers', hidden: true },
    h('div', { class: 'setting-label' }, h('span', {}, 'Choose a server'), h('p', { class: 'hint' }, 'The account owns several.')),
    h('span', { class: 'secret', dataset: { list: '' } }));
  const fragment = document.createDocumentFragment();
  fragment.append(account, servers);
  return fragment;
}

/** Sections the server offers for films or series; the saved one until they load. */
function sectionOptions(select, kind) {
  const value = select.dataset.shown;
  const sections = (settings.plexSections ?? []).filter((s) => s.type === kind);
  const options = sections.some((s) => s.key === value) || !value ? sections : [{ key: value, title: `Section ${value}` }, ...sections];
  select.replaceChildren(...options.map((s) => h('option', { value: s.key, selected: s.key === value }, s.title)));
}

async function loadPlexSections() {
  if (!plexConnected()) return;
  try {
    settings.plexSections = itemsOf((await link.http('GET', '/api/plex/sections')).sections);
  } catch { return; /* the saved numbers stay shown */ }
  for (const select of $('#settings-form').querySelectorAll('select[data-kind]')) sectionOptions(select, select.dataset.kind);
}

async function connectPlex() {
  if (!$('#save-bar').hidden) throw new Error('Save or discard your changes first');
  // The window opens on the click itself, or the browser blocks it.
  const popup = open('', 'plex-sign-in', 'popup,width=560,height=720');
  if (!popup) throw new Error('Allow pop-ups for Mynou, then connect again');
  try {
    const pin = await link.http('POST', '/api/plex/pin', {});
    popup.location = pin.url;
    const until = Date.now() + 10 * 60_000;
    for (;;) {
      await sleep(1500);
      const closed = popup.closed;
      if ((await link.http('GET', `/api/plex/pin/${pin.id}`)).signed_in) break;
      if (closed) throw new Error('Plex sign-in cancelled');
      if (Date.now() > until) throw new Error('The Plex sign-in expired: try again');
    }
  } finally { popup.close(); }
  settings.secrets = itemsOf((await link.call('GET', '/api/secrets')).secrets);
  await chooseServer();
}

async function chooseServer() {
  if (!$('#save-bar').hidden) throw new Error('Save or discard your changes first');
  const servers = itemsOf((await link.http('GET', '/api/plex/servers')).servers);
  if (!servers.length) throw new Error('This Plex account owns no server');
  if (servers.length === 1) { await useServer(servers[0].id); return; }
  const row = $('#plex-servers');
  $('[data-list]', row).replaceChildren(...servers.map((server) =>
    h('button', { type: 'button', class: server.current ? '' : 'secondary', dataset: { plexServer: server.id } }, server.name)));
  row.hidden = false;
  Motion.animate(row, [{ opacity: 0, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
}

async function useServer(id) {
  const reply = await link.http('POST', '/api/plex/server', { id });
  await applied(reply);
  settings.plexSections = null;
  await loadSettings();
  const found = [reply.movies_section && `films in ${reply.movies_section}`, reply.series_section && `series in ${reply.series_section}`].filter(Boolean);
  toast(`Connected to ${reply.name}${found.length ? `: ${found.join(', ')}` : ''}`);
}

async function plexAction(button) {
  const action = button.dataset.plexAct;
  if (action === 'connect') await connectPlex();
  else if (action === 'servers') await chooseServer();
  else if (action === 'disconnect') {
    if (!$('#save-bar').hidden) throw new Error('Save or discard your changes first');
    if (!confirm('Disconnect Plex? Mynou stops filing into it and reading the watchlist.')) return;
    await applied(await link.http('POST', '/api/plex/disconnect', {}));
    await loadSettings();
    toast('Plex disconnected');
  }
}

function settingRow([path, label, type, options = {}]) {
  const value = settingValue(path);
  const id = `setting-${path.replaceAll('.', '-')}`;
  const source = type === 'secret' ? secretSource(String(value ?? '')) : null;
  const text = h('div', { class: 'setting-label' },
    h('label', { htmlFor: id }, label, source ? h('span', { class: 'badge', dataset: { tone: SOURCE_TONES[source] } }, SOURCE_LABELS[source]) : null),
    options.hint ? h('p', { class: 'hint' }, options.hint) : null);
  let control;
  if (type === 'plex') return plexRows();
  if (type === 'section') {
    const select = h('select', { id, dataset: { path, type: 'text', kind: options.kind, shown: String(value ?? '') } });
    sectionOptions(select, options.kind);
    return h('div', { class: 'setting-row', hidden: !plexConnected() }, text, select);
  }
  if (type === 'sync') {
    return h('div', { class: 'setting-row toggle', id: 'plex-watchlist', hidden: !plexConnected() },
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
      value: display, placeholder: options.zero ?? (options.folder ? '/Volumes/Media' : ''), spellcheck: false,
      dataset: { path, type, shown: display, scale: options.scale ?? 1, zero: options.zero ? '1' : '', folder: options.folder ? '1' : '' },
    });
    control = options.unit ? h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, options.unit)) : input;
  }
  return h('div', { class: `setting-row${type === 'check' ? ' toggle' : ''}`, dataset: { when: options.when ?? '' } }, text, control);
}

/** A setting row around a control the caller built. */
const plainRow = (id, label, hint, control, toggle = false) => h('div', { class: `setting-row${toggle ? ' toggle' : ''}` },
  h('div', { class: 'setting-label' }, h('label', { htmlFor: id }, label), hint ? h('p', { class: 'hint' }, hint) : null), control);

/** Fields that depend on a switch only show while it is on. */
function syncDependents() {
  for (const row of $('#settings-form').querySelectorAll('[data-when]:not([data-when=""])')) {
    row.hidden = !$(`[data-path="${row.dataset.when}"]`)?.checked;
  }
}

// Rules of one indexer: [field, label, hint, scale, unit, empty]. An empty
// field keeps the default, which the placeholder names.
const INDEXER_RULES = [
  ['priority', 'Priority', 'Between equally good releases, the lower number wins.', 1, '', 'Default, 25'],
  ['minimum_seeders', 'Minimum seeders', 'Releases with fewer seeders are skipped.', 1, '', null],
  ['seed_ratio_milli', 'Seed until ratio', 'For downloads from this indexer.', 1000, '', 'Default'],
  ['seed_time_secs', 'Seed at least', 'For downloads from this indexer.', 3600, 'h', 'Default'],
];

function indexerRules(indexer) {
  const changed = INDEXER_RULES.some(([field]) => indexer[field] != null) || indexer.prefer_freeleech;
  return h('details', { class: 'indexer-rules', open: Boolean(changed) },
    h('summary', {}, 'Rules for this indexer'),
    h('div', { class: 'indexer-rules-grid' },
      INDEXER_RULES.map(([field, label, hint, scale, unit, empty]) => {
        const value = indexer[field] == null ? '' : String(Math.round((Number(indexer[field]) / scale) * 1000) / 1000);
        const input = h('input', { type: 'text', inputMode: scale === 1 ? 'numeric' : 'decimal', value, 'aria-label': label, title: hint,
          placeholder: empty ?? `As in Downloads (${settingValue('minimum_seeders') ?? 1})`, dataset: { rule: field, scale } });
        return h('label', { class: 'field' }, label, unit ? h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, unit)) : input);
      }),
      h('label', { class: 'field toggle-field' },
        h('input', { type: 'checkbox', class: 'switch', checked: Boolean(indexer.prefer_freeleech), dataset: { rule: 'prefer_freeleech' } }),
        'Prefer freeleech', h('span', { class: 'hint' }, 'Freeleech downloads do not count against your ratio on this tracker.'))));
}

/** The rules typed in an indexer row, folded into its settings. */
function readIndexerRules(row, indexer) {
  const name = indexer.name || indexer.url || 'This indexer';
  for (const input of row.querySelectorAll('[data-rule]')) {
    const field = input.dataset.rule;
    if (input.type === 'checkbox') {
      if (input.checked) indexer[field] = true; else delete indexer[field];
      continue;
    }
    const typed = input.value.trim().replace(',', '.');
    if (!typed) { delete indexer[field]; continue; }
    const value = Math.round(Number(typed) * Number(input.dataset.scale));
    if (!Number.isFinite(value) || value < 0) throw new Error(`${name}: ${input.getAttribute('aria-label')} needs a number`);
    if (field === 'priority' && (value < 1 || value > 50)) throw new Error(`${name}: priority goes from 1 to 50`);
    indexer[field] = value;
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
    indexerRules(indexer),
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
      // Folders are on the Mac running Mynou, wherever this page is open.
      if (input.dataset.folder && !/^~?\//.test(value)) throw new Error(`${$(`label[for="${input.id}"]`).textContent} needs a full path on the Mac running Mynou, starting with /`);
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
  readQuality(document, $('#settings-form'));
  document.indexers = [...$('#settings-indexers').children].map((row) => {
    const indexer = { ...row._indexer };
    for (const input of row.querySelectorAll('[data-field]')) {
      indexer[input.dataset.field] = input.type === 'checkbox' ? input.checked : input.value.trim();
    }
    readIndexerRules(row, indexer);
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
  const open = Boolean(getPath(settings.document, 'access.local_network_open'));
  const state = settings.report.restart_required ? 'Restart Mynou to apply the change.'
    : exposed(access.listen) ? (address ? `From another computer, open ${address}.` : 'Other computers can open Mynou.')
    : 'Only this Mac can open Mynou.';
  const row = (label, hint, control, toggle = false) => h('div', { class: `setting-row${toggle ? ' toggle' : ''}` },
    h('div', { class: 'setting-label' }, label, hint ? h('p', { class: 'hint' }, hint) : null), control);
  const badge = h('span', { class: 'badge', dataset: { tone: access.password_set ? 'ok' : 'warn' } }, access.password_set ? 'Set' : 'Not set');
  return [
    row(h('label', { htmlFor: 'network-on' }, 'Allow other computers'), state,
      h('input', { id: 'network-on', type: 'checkbox', class: 'switch', checked: on, disabled: !mac, dataset: { shown: String(on) } }), true),
    row(h('label', { htmlFor: 'network-open' }, 'No password on the local network'), 'Computers at home or at the office open Mynou directly. Any other one still needs the password.',
      h('input', { id: 'network-open', type: 'checkbox', class: 'switch', checked: open, disabled: !mac, dataset: { shown: String(open) } }), true),
    row(h('label', { htmlFor: 'network-port' }, 'Port'), 'With 80, addresses need no port.',
      h('input', { id: 'network-port', type: 'text', inputMode: 'numeric', value: port, disabled: !mac, spellcheck: false, dataset: { shown: port } })),
    row(h('label', { htmlFor: 'network-hosts' }, 'Other host names'), `Mynou already answers this Mac's addresses${access.local_name ? ` and ${access.local_name}` : ''}. Add other names, separated by commas.`,
      h('input', { id: 'network-hosts', type: 'text', value: hosts, disabled: !mac, spellcheck: false, placeholder: 'None', dataset: { shown: hosts } })),
    row(h('label', { htmlFor: 'network-password' }, 'Password', badge), 'At least 10 characters. A new password signs every other computer out.',
      h('span', { class: 'secret' },
        h('input', { id: 'network-password', type: 'password', autocomplete: 'new-password', maxLength: 1024, disabled: !mac, placeholder: access.password_set ? 'Type to replace' : 'Choose a password' }),
        h('button', { type: 'button', class: 'secondary', disabled: !mac, dataset: { networkAct: 'password' } }, 'Save password'),
        access.password_set && (!on || open) && mac ? h('button', { type: 'button', class: 'ghost danger', dataset: { networkAct: 'remove' } }, 'Remove') : null)),
    access.password_set && mac ? row(h('span', {}, 'Signed-in computers'), 'Sign out every other computer and phone, for example after losing one. The password stays the same.',
      h('button', { type: 'button', class: 'ghost danger', dataset: { networkAct: 'sign-out-everywhere' } }, 'Sign out everywhere')) : null,
    mac ? null : row(h('span', {}, 'This computer'), 'Network access changes only on the Mac running Mynou.',
      h('button', { type: 'button', class: 'ghost', dataset: { networkAct: 'sign-out' } }, 'Sign out')),
  ].filter(Boolean);
}

/** Folds the switch, port and host names into the document, when changed. */
function readNetwork(document) {
  const on = $('#network-on');
  if (!on || on.disabled) return;
  const port = $('#network-port').value.trim();
  const open = $('#network-open');
  if (String(on.checked) !== on.dataset.shown || port !== $('#network-port').dataset.shown) {
    if (!/^\d{1,5}$/.test(port) || Number(port) < 1 || Number(port) > 65535) throw new Error('Port needs a number from 1 to 65535');
    if (on.checked && !open.checked && !settings.access.password_set) throw new Error('Save a password or turn on No password on the local network, then allow other computers');
    document.listen = `${on.checked ? '0.0.0.0' : '127.0.0.1'}:${Number(port)}`;
  }
  if (String(open.checked) !== open.dataset.shown) setPath(document, 'access.local_network_open', open.checked);
  const hosts = $('#network-hosts');
  if (hosts.value.trim() !== hosts.dataset.shown) {
    setPath(document, 'access.allowed_hosts', hosts.value.split(/[\s,]+/).filter(Boolean));
  }
}

async function networkAction(button) {
  const action = button.dataset.networkAct;
  if (action === 'sign-out') { await signOut(); return; }
  if (action === 'sign-out-everywhere') {
    if (!confirm('Sign out every other computer and phone?')) return;
    settings.access = await link.http('POST', '/api/access', { sign_out_everywhere: true });
    toast('Every other computer is signed out');
    return;
  }
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

/** Waits until the service applied a settings change. */
async function applied(reply) {
  if (reply.state === 'applying') {
    settingsState('Applying the new settings…');
    // The service restarts its engine; wait until the change has landed.
    for (let i = 0; i < 60; i++) {
      await sleep(1000);
      const report = await link.call('GET', '/api/settings').catch(() => null);
      if (report && !report.applying) {
        if (report.error) throw new Error(report.error);
        break;
      }
    }
  }
}

async function saveSettings(document) {
  const reply = await link.call('POST', '/api/settings', { settings: document });
  await applied(reply);
  // Profile pickers elsewhere read the saved profiles again.
  forgetProfiles();
  await loadSettings();
  toast(reply.restart_required ? 'Settings saved; restart Mynou to apply them' : 'Settings applied');
}

// VPN: its controls apply at once, outside the settings document and Save.
const VPN_STATES = {
  protected: ['Protected', 'ok'], connecting: ['Connecting', 'warn'], reconnecting: ['Reconnecting', 'warn'],
  disabled: ['Off', 'warn'], blocked: ['Blocked', 'bad'], error: ['Error', 'bad'],
};
const VPN_DONE = {
  enable: 'VPN turned on: connecting', disable: 'VPN turned off: everything uses your own connection',
  move: 'Order saved: reconnecting', remove: 'Profile removed',
};
const VPN_REASONS = {
  GaveUp: 'The VPN server never answered. Check the profile, or try another server.',
  Expired: 'The session expired.', Failed: 'The tunnel failed.', EndpointChanged: 'The server address changed.',
  Cancelled: 'Stopped.', Exhausted: 'Every profile failed.',
};
// Why the tunnel left a profile for the next one.
const VPN_SWITCHES = { GaveUp: 'Its server stopped answering', Expired: 'Its session expired', Failed: 'The tunnel failed' };
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
      h('div', { class: 'setting-label' }, label('WireGuard profiles'), h('p', { class: 'hint' })),
      h('button', { type: 'button', class: 'secondary', dataset: { vpnAct: 'import' } }),
      h('input', { type: 'file', accept: '.conf', hidden: true, dataset: { vpn: 'file' } }),
      h('ol', { class: 'vpn-profiles', dataset: { vpn: 'list' } })),
    h('div', { class: 'setting-row toggle', dataset: { vpn: 'switches' } },
      h('div', { class: 'setting-label' }, h('span', {}, 'Recent switches'), h('ol', { class: 'vpn-switches' }))),
    h('div', { class: 'setting-row toggle', dataset: { vpn: 'forwarding' } },
      h('div', { class: 'setting-label' }, h('label', { htmlFor: 'vpn-forwarding' }, 'Open a port for seeding'), h('p', { class: 'hint' })),
      h('input', { id: 'vpn-forwarding', type: 'checkbox', class: 'switch' })),
  ];
}

/** The profiles in the order they are tried, each movable and removable. */
function paintVpnProfiles(list, vpn) {
  const profiles = itemsOf(vpn?.profiles);
  // Repainted only when it changed, so a push never swaps a button being clicked.
  const signature = JSON.stringify([profiles, vpn?.enabled, vpn?.state]);
  if (list.dataset.signature === signature) return;
  list.dataset.signature = signature;
  // The last profile goes once the VPN is off: protected traffic never goes direct.
  const locked = profiles.length === 1 && Boolean(vpn?.enabled);
  list.replaceChildren(...profiles.map((profile, index) => h('li', { class: profile.active ? 'on' : '' },
    h('span', { class: 'vpn-rank' }, String(index + 1)),
    h('span', { class: 'vpn-name' }, h('strong', {}, profile.name),
      profile.active ? h('small', {}, vpn?.state === 'protected' ? 'In use' : 'Trying now') : null),
    h('span', { class: 'ladder-moves' },
      h('button', { type: 'button', class: 'icon-button', 'aria-label': `Try ${profile.name} earlier`, disabled: index === 0, dataset: { vpnAct: 'move', index, to: index - 1 } }, '↑'),
      h('button', { type: 'button', class: 'icon-button', 'aria-label': `Try ${profile.name} later`, disabled: index === profiles.length - 1, dataset: { vpnAct: 'move', index, to: index + 1 } }, '↓'),
      h('button', {
        type: 'button', class: 'icon-button danger', 'aria-label': `Remove ${profile.name}`, disabled: locked,
        title: locked ? 'Turn the VPN off before removing its last profile' : '', dataset: { vpnAct: 'remove', index, name: profile.name },
      }, icon('close'))))));
}

/** The tunnel as the last status push reports it. */
function paintVpn() {
  const list = $('#settings-vpn');
  if (!list.childElementCount) list.replaceChildren(...vpnRows());
  const vpn = data.get('status')?.data?.vpn;
  const row = (name) => $(`[data-vpn="${name}"]`, list);
  const external = vpn?.state === 'external';
  const profile = Boolean(vpn?.profile_present);
  const profiles = itemsOf(vpn?.profiles);
  const revoked = vpn?.reason === VPN_REVOKED;
  // Every profile failed in a row: Mynou waits, then tries the list again.
  const resting = vpn?.state === 'reconnecting' && vpn.reason === 'Exhausted';
  const [state, tone] = !vpn ? ['…', 'warn'] : external ? ['Not managed here', 'warn']
    : vpn.reason === VPN_RESTARTING ? ['Restarting', 'warn'] : resting ? ['Waiting', 'bad']
    : VPN_STATES[vpn.state] ?? [sentence(vpn.state), 'warn'];
  const badge = $('.badge', row('state'));
  badge.textContent = state;
  badge.dataset.tone = tone;
  const age = Number(vpn?.handshake_age_secs);
  const through = profiles.length > 1 && vpn?.active_profile ? `Through ${vpn.active_profile}. ` : '';
  $('.hint', row('state')).textContent = external ? 'This Mynou uses an outside network; it runs no VPN of its own.'
    : vpn?.state === 'protected' ? `${through}${vpn.ipv6 ? 'IPv4 and IPv6.' : 'IPv4 only.'} ${Number.isFinite(age) && vpn.handshake_age_secs !== null ? `Last handshake ${age} s ago.` : 'Connected.'}`
    : vpn?.state === 'disabled' ? (profile ? 'Everything uses your own connection.' : 'Import a profile to use a VPN.')
    : resting ? `${profiles.length > 1 ? 'Every profile failed' : 'The VPN server never answered'}, so torrents wait. Next try ${relativeTime(Date.now() / 1000 + (Number(vpn.retry_in_secs) || 0))}`
    : vpn?.state === 'connecting' ? `${through}Waiting for the VPN server to answer.`
    : vpn?.reason && vpn.reason !== VPN_RESTARTING ? VPN_REASONS[vpn.reason] ?? vpn.reason : '';
  // A failed tunnel is retried from here: turning it off would send torrents direct.
  const failed = vpn?.enabled && !revoked && (resting || vpn.state === 'error' || (vpn.state === 'blocked' && vpn.reason !== VPN_RESTARTING));
  const enable = $('[data-vpn-act="enable"]', list);
  enable.hidden = external || !profile || (vpn.enabled && !revoked && !failed);
  enable.textContent = failed ? 'Try again' : 'Turn on';
  $('[data-vpn-act="disable"]', list).hidden = external || !vpn?.enabled;
  // The choices only matter while the VPN is on.
  $('#settings-vpn-uses').hidden = external || !vpn?.enabled;
  row('profile').hidden = !vpn || external;
  const imported = $('.badge', row('profile'));
  imported.textContent = profiles.length > 1 ? `${profiles.length} profiles` : profile ? 'Imported' : 'Not imported';
  imported.dataset.tone = profile ? 'ok' : 'warn';
  $('.hint', row('profile')).textContent = profiles.length > 1
    ? 'Tried in this order. When a server stops answering for about five minutes, the next profile takes over.'
    : 'The WireGuard .conf file from your provider. Add others to fall back on when its server stops answering.';
  $('[data-vpn-act="import"]', list).textContent = profile ? 'Add a .conf' : 'Import a .conf';
  paintVpnProfiles($('[data-vpn="list"]', list), vpn);
  const switches = itemsOf(vpn?.switches);
  row('switches').hidden = external || !switches.length;
  $('.vpn-switches', list).replaceChildren(...switches.map((change) => h('li', {},
    h('span', {}, `${change.from ?? '?'} → ${change.to ?? '?'}`),
    h('small', {}, `${relativeTime(change.at)} · ${VPN_SWITCHES[change.reason] ?? sentence(change.reason)}`))));
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

// Plex friends: each switch applies at once, outside the settings document
// and Save. New friends are off.
const FRIEND_STATES = {
  on: ['Following', 'ok'], reading: ['Reading', 'warn'], off: ['Off', ''], error: ['Error', 'bad'],
  left: ['No longer a friend', 'warn'], duplicate: ['Already a requester', 'warn'],
};
// Plex is asked for the list when the page opens, at most this often.
const FRIENDS_STALE_MS = 10 * 60_000;
const friends = { askedAt: 0, asking: false };

async function refreshRequesters() {
  const body = await link.call('GET', '/api/requesters');
  data.set('requesters', { status: 200, data: body, at: Date.now() });
  paintFriends();
}

async function checkFriends() {
  friends.asking = true;
  friends.askedAt = Date.now();
  try {
    await link.call('POST', '/api/plex/friends', {});
  } finally {
    friends.asking = false;
    await refreshRequesters().catch(() => {});
  }
}

function friendRow(friend) {
  const id = `friend-${friend.id}`;
  const [state, tone] = FRIEND_STATES[friend.state] ?? [sentence(friend.state), 'warn'];
  const titles = Number(friend.titles) || 0;
  const name = friend.name || friend.username || 'Plex friend';
  const hint = friend.state === 'left' ? 'Their watchlist is no longer read; what was requested stays.'
    : friend.error ? plexError(friend.error)
    : friend.state === 'on' ? `${titles.toLocaleString()} ${titles === 1 ? 'title' : 'titles'} on the watchlist.`
    : friend.state === 'reading' ? 'Reading the watchlist for the first time.'
    : friend.username && friend.username !== name ? friend.username : '';
  // A friend who left, or who already has a requester account, cannot be
  // turned on; one still on can be turned off.
  const locked = !friend.enabled && (friend.state === 'left' || friend.state === 'duplicate');
  return h('div', { class: 'setting-row toggle', dataset: { friend: friend.id } },
    h('div', { class: 'setting-label' },
      h('label', { htmlFor: id }, name, h('span', { class: 'badge', dataset: { tone } }, state)),
      hint ? h('p', { class: `hint${friend.state === 'error' ? ' bad' : ''}` }, hint) : null),
    h('input', { id, type: 'checkbox', class: 'switch', checked: Boolean(friend.enabled), disabled: locked, dataset: { friendSwitch: friend.id } }));
}

/** A requester account with its own Plex token, shown beside the friends. */
function accountRow(account) {
  const on = Boolean(account.policy?.enabled);
  const error = account.last_error;
  return h('div', { class: 'setting-row toggle' },
    h('div', { class: 'setting-label' },
      h('span', {}, account.id, h('span', { class: 'badge', dataset: { tone: error ? 'bad' : on ? 'ok' : '' } }, error ? 'Error' : on ? 'Following' : 'Off')),
      h('p', { class: `hint${error ? ' bad' : ''}` }, error ? plexError(error) : 'A requester account with its own Plex token.')));
}

/** The friend list as the last requesters push reports it. */
function paintFriends() {
  const list = $('#settings-friends');
  if (!list) return;
  const requesters = data.get('requesters')?.data;
  const plex = requesters?.plex;
  const listed = itemsOf(plex?.friends);
  const accounts = itemsOf(requesters?.accounts).filter((a) => !a.plex && !a.id.startsWith('plex-'));
  const checked = Number(plex?.checked_at) || 0;
  const count = listed.filter((f) => f.state !== 'left').length;
  const hint = !requesters ? '…'
    : !plex?.connected ? 'Connect Plex above, then your friends show here.'
    : plex.error ? plexError(plex.error)
    : friends.asking && !checked ? 'Asking Plex…'
    : checked ? `${count ? `${count.toLocaleString()} ${count === 1 ? 'friend' : 'friends'}` : 'No friends'} on Plex, checked ${relativeTime(checked)}.`
    : 'Not checked yet.';
  // A switch waiting for its answer is repainted once the answer is in.
  if (list.querySelector('[data-friend-switch][data-busy]')) return;
  const head = h('div', { class: 'setting-row toggle' },
    h('div', { class: 'setting-label' }, h('span', {}, 'Plex friends'), h('p', { class: `hint${plex?.error ? ' bad' : ''}` }, hint)),
    h('button', { type: 'button', class: 'secondary', disabled: !plex?.connected || friends.asking, dataset: { friendsAct: 'check' } }, icon('sync'), 'Check now'));
  list.replaceChildren(head, ...listed.map(friendRow), ...accounts.map(accountRow));
  // Opening the page asks Plex for the list when it is old.
  if (currentParam() === 'services' && plex?.connected && !friends.asking
    && Date.now() - checked * 1000 > FRIENDS_STALE_MS && Date.now() - friends.askedAt > FRIENDS_STALE_MS) {
    checkFriends().catch(() => {});
  }
}

async function switchFriend(input) {
  const on = input.checked;
  input.disabled = true;
  input.dataset.busy = '1';
  try {
    const friend = await link.call('POST', `/api/plex/friends/${encodeURIComponent(input.dataset.friendSwitch)}`, { enabled: on });
    toast(on ? `Following ${friend.name}’s watchlist` : `${friend.name}’s watchlist is no longer read; what was requested stays`);
  } catch (e) {
    input.checked = !on;
    toast(e.message, 'bad');
  } finally {
    delete input.dataset.busy;
    input.disabled = false;
    await refreshRequesters().catch(() => {});
  }
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
      h('a', { href: `#settings/${page}`, dataset: { page } }, h('span', {}, title), icon('chevron'))));
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
      if (action === 'disable' && !confirm('Turn off the VPN? Everything will then use your own connection.')) return;
      if (action === 'remove' && !confirm(`Remove the profile ${button.dataset.name}? Mynou forgets its keys.`)) return;
      const body = { action };
      // Profiles are counted from 0, in the order they are tried.
      if (action === 'move' || action === 'remove') body.index = Number(button.dataset.index);
      if (action === 'move') body.to = Number(button.dataset.to);
      act(button, () => vpnControl(body), VPN_DONE[action]);
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
          // The file's name is the profile's, such as the server it leads to.
          await vpnControl({ action: 'import', profile: await file.text(), name: file.name.replace(/\.conf$/i, '') });
        }, 'Profile added: connecting');
      } else if (input.id === 'vpn-forwarding') {
        const on = input.checked;
        input.disabled = true;
        vpnControl({ action: on ? 'enable-forwarding' : 'disable-forwarding' })
          .then(() => toast(on ? 'Asking the VPN server for a port' : 'Port closed'))
          .catch((e) => { input.checked = !on; toast(e.message, 'bad'); })
          .finally(() => { input.disabled = false; });
      }
    });
    const friendList = $('#settings-friends');
    // Friend switches apply at once: they never mark the settings unsaved.
    friendList.addEventListener('input', (event) => event.stopPropagation());
    friendList.addEventListener('change', (event) => {
      event.stopPropagation();
      if (event.target.dataset.friendSwitch) switchFriend(event.target);
    });
    friendList.addEventListener('click', (event) => {
      const button = event.target.closest('[data-friends-act="check"]');
      if (button) act(button, checkFriends);
    });
    $('#indexer-add').addEventListener('click', () => {
      const row = indexerRow({ kind: 'torznab', enabled: true });
      $('#settings-indexers').append(row);
      Motion.animate(row, [{ opacity: 0, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
      $('input', row).focus();
      dirty();
    });
    form.addEventListener('click', (event) => {
      const plex = event.target.closest('[data-plex-act]');
      if (plex) { act(plex, () => plexAction(plex)); return; }
      const server = event.target.closest('[data-plex-server]');
      if (server) { act(server, () => useServer(server.dataset.plexServer)); return; }
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
  page: () => $('#settings-form'),
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
    const group = (page, title, about, rows) => h('section', { class: 'setting-group', dataset: { page } },
      h('div', { class: 'setting-intro' }, h('h2', {}, title), h('p', { class: 'hint' }, about)),
      h('div', { class: 'setting-list' }, rows));
    $('#settings-groups').replaceChildren(
      ...SETTING_GROUPS.map(([page, title, about, fields]) => group(page, title, about, fields.map(settingRow))),
      ...qualityGroups(settings.document, plainRow).map(([title, about, rows]) => group('quality', title, about, rows)));
    // Saved indexers keep their position, which is also the order of the live reports.
    $('#settings-indexers').replaceChildren(...itemsOf(settings.document.indexers).map((indexer, i) => Object.assign(indexerRow(indexer), { _saved: i })));
    $('#settings-network').replaceChildren(...networkRows());
    $('#settings-vpn-uses').replaceChildren(...VPN_USES.map(settingRow));
    $('#save-bar').hidden = true;
    syncDependents();
    showPage();
    this.paintLive();
    loadPlexSections();
  },
  /** States that change while the page is open; inputs are never touched. */
  paintLive() {
    paintVpn();
    paintFriends();
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
      : accounts ? `${accounts} ${accounts === 1 ? 'watchlist is' : 'watchlists are'} read every minute.` : 'Your watchlist is read every minute.';
    hint.classList.toggle('bad', Boolean(status.last_sync_error));
  },
};
