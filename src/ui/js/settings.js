import { sentence, relativeTime } from './format.js';
import { $, h, icon } from './dom.js';
import { toast, act, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { data, views, itemsOf, currentParam, currentView } from './store.js';
import { INDEXER_ERRORS, plexError, indexerReports } from './sources.js';
import { signOut } from './session.js';
import { qualityGroups, readQuality, forgetProfiles, qualityEdits } from './quality.js';

// The settings document keeps every field; the groups below edit the common
// ones in place and leave the others untouched. Saving sends the whole document,
// which the service validates and applies by restarting its engine.
// Settings come in pages, #settings/<page>, all in one form so a change on one
// page survives a visit to another until it is saved or discarded.
// A page is [id, title, icon, starts a new set in the list].
const SETTING_PAGES = [
  ['library', 'Library', 'library'], ['downloads', 'Downloads', 'transfers'], ['quality', 'Quality', 'quality'], ['indexers', 'Indexers', 'search'],
  ['services', 'Plex & TMDB', 'series', true], ['vpn', 'VPN', 'shield'], ['network', 'Network', 'network'],
  ['appearance', 'Appearance', 'auto', true], ['about', 'About', 'info'],
];
const PAGE_TITLES = new Map(SETTING_PAGES.map(([id, title]) => [id, title]));
// The page that edits each part of the settings document, to say where changes wait.
const PAGE_OF = {
  library: 'library', monitoring: 'library', series_packs: 'library', updates: 'library', downloads: 'downloads', wanted: 'downloads',
  selection: 'quality', indexers: 'indexers', plex: 'services', catalog: 'services', vpn: 'vpn', listen: 'network', access: 'network',
};
const pageOf = (el) => el?.closest('[data-page]')?.dataset.page ?? null;

// Live paints write only what changed, so a push every second costs nothing
// when nothing moved, and never disturbs a pointer or a focus.
function setText(el, text) { if (el && el.textContent !== text) el.textContent = text; }
function setData(el, key, value) { if (el && el.dataset[key] !== value) el.dataset[key] = value; }
function setShown(el, shown) { if (el && el.hidden === shown) el.hidden = !shown; }
function setFlag(el, name, on) { if (el && el.hasAttribute(name) !== on) el.toggleAttribute(name, on); }

/** A value the form refuses, with the field to show it under. */
class FieldError extends Error {
  constructor(input, message) { super(message); this.input = input; }
}
const check = (input) => { const problem = input?._check?.(); if (problem) throw new FieldError(input, problem); };
// A group is [page, title, description, fields]; a field is [path, label, type, options]: `hint` explains it, `when` names the
// switch it depends on, `scale` and `unit` show a stored number in friendlier
// units, `zero` is what an empty field means, and `range` is what the service
// accepts, stored units, so a value out of it is told before saving.
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
  ['downloads', 'Wanted titles', 'Films and episodes without an acceptable release yet. Feeds and IRC take one as soon as it appears; Mynou also searches for them again, slowly.', [
    ['wanted.interval_secs', 'Search each title every', 'number', { scale: 3600, unit: 'h', range: [3600, 2_592_000], hint: 'From 1 hour to 30 days.' }],
    ['wanted.titles_per_pass', 'Titles searched per hour', 'number', { range: [1, 120], hint: 'From 1 to 120, spread evenly over the hour so indexers never see a burst.' }],
    ['wanted.theatrical_delay_days', 'Wait after a cinema release', 'number', { unit: 'days', range: [0, 730], hint: 'A film is searched once it is out digitally, on disc or on TV, or this long after its first cinema release, up to 730 days.' }],
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
    ['monitoring.interval_secs', 'Check every', 'number', { when: 'monitoring.enabled', scale: 60, unit: 'min', range: [60, 86_400] }],
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
// What the form would save as painted, the pages whose settings differ from it,
// and whether anything waits to be saved.
const edits = { baseline: null, pages: new Set(), dirty: false };

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
        ? h('button', { type: 'button', class: 'primary', dataset: { plexAct: 'connect' } }, 'Connect to Plex')
        : [h('button', { type: 'button', class: 'primary', dataset: { plexAct: 'servers' } }, 'Choose a server'),
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
  if (edits.dirty) throw new Error('Save or discard your changes first');
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
  if (edits.dirty) throw new Error('Save or discard your changes first');
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
    if (edits.dirty) throw new Error('Save or discard your changes first');
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
    input._check = type === 'number' ? () => numberProblem(input, options) : options.folder ? () => folderProblem(input, value) : null;
    control = options.unit ? h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, options.unit)) : input;
  }
  // Paths and keys are long: they take the whole width under their label.
  const wide = options.folder || type === 'secret';
  return h('div', { class: `setting-row${type === 'check' ? ' toggle' : ''}${wide ? ' wide' : ''}`, dataset: { when: options.when ?? '' } }, text, control);
}

// What a typed value lacks, or null; an untouched field is never questioned.
function numberProblem(input, { range, scale = 1, unit } = {}) {
  const raw = input.value.trim();
  if (raw === input.dataset.shown || (raw === '' && input.dataset.zero)) return null;
  const value = Math.round(Number(raw.replace(',', '.')) * scale);
  if (raw === '' || !Number.isFinite(value) || value < 0) return 'Type a number';
  if (range && (value < range[0] || value > range[1])) return `From ${range[0] / scale} to ${range[1] / scale}${unit ? ` ${unit}` : ''}`;
  return null;
}
function folderProblem(input, saved) {
  const typed = input.value.trim();
  if (typed === input.dataset.shown || (!typed && saved === undefined)) return null;
  // Folders are on the Mac running Mynou, wherever this page is open.
  return /^~?\//.test(typed) ? null : 'A full path on the Mac running Mynou, starting with /';
}

/** A setting row around a control the caller built. */
const plainRow = (id, label, hint, control, toggle = false) => h('div', { class: `setting-row${toggle ? ' toggle' : ''}` },
  h('div', { class: 'setting-label' }, h('label', { htmlFor: id }, label), hint ? h('p', { class: 'hint' }, hint) : null), control);

/** Fields that depend on a switch only show while it is on; one turned on slides in. */
function syncDependents(animate = false) {
  for (const row of $('#settings-form').querySelectorAll('[data-when]:not([data-when=""])')) {
    const on = Boolean($(`[data-path="${row.dataset.when}"]`)?.checked);
    if (row.hidden !== on) continue;
    row.hidden = !on;
    if (on && animate) Motion.animate(row, [{ opacity: 0, transform: 'translateY(-6px)' }, { opacity: 1, transform: 'none' }], { duration: 320, easing: Motion.spring() });
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
        input._check = () => ruleProblem(input);
        return h('label', { class: 'field' }, label, unit ? h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, unit)) : input);
      }),
      h('label', { class: 'field toggle-field' },
        h('input', { type: 'checkbox', class: 'switch', checked: Boolean(indexer.prefer_freeleech), dataset: { rule: 'prefer_freeleech' } }),
        'Prefer freeleech', h('span', { class: 'hint' }, 'Freeleech downloads do not count against your ratio on this tracker.'))));
}

const ruleValue = (input) => Math.round(Number(input.value.trim().replace(',', '.')) * Number(input.dataset.scale));
function ruleProblem(input) {
  if (!input.value.trim()) return null;
  const value = ruleValue(input);
  if (!Number.isFinite(value) || value < 0) return 'Type a number';
  return input.dataset.rule === 'priority' && (value < 1 || value > 50) ? 'From 1 to 50' : null;
}

/** The rules typed in an indexer row, folded into its settings. */
function readIndexerRules(row, indexer) {
  for (const input of row.querySelectorAll('[data-rule]')) {
    const field = input.dataset.rule;
    if (input.type === 'checkbox') {
      if (input.checked) indexer[field] = true; else delete indexer[field];
      continue;
    }
    if (!input.value.trim()) { delete indexer[field]; continue; }
    check(input);
    indexer[field] = ruleValue(input);
  }
}

/** A web address the page may open: http or https only, never `javascript:`.
    The scheme is written here rather than copied from the field, so what the
    field holds can only ever follow it. */
function webAddress(text) {
  if (!/^https?:\/\/[^\s/?#@]+/i.test(text)) return null;
  return (/^https:/i.test(text) ? 'https:' : 'http:') + text.slice(text.indexOf(':') + 1);
}

function indexerRow(indexer) {
  const keyName = indexer.api_key_env ?? 'MYNOU_INDEXER_API_KEY';
  // The indexer's own website, opened from here in a new tab.
  const visit = h('a', { class: 'icon-button', target: '_blank', rel: 'noopener noreferrer', title: 'Open the website' }, icon('arrow'));
  const site = h('input', { type: 'url', class: 'indexer-site', value: indexer.site_url ?? '', maxLength: 2048, placeholder: 'Website, to open it from here (optional)',
    'aria-label': 'Indexer website', spellcheck: false, dataset: { field: 'site_url' } });
  site._check = () => (site.value.trim() && !webAddress(site.value.trim()) ? 'A full address, as in the address bar of a browser' : null);
  const address = h('input', { type: 'url', value: indexer.url ?? '', required: true, placeholder: 'Indexer address', 'aria-label': 'Indexer URL', spellcheck: false, dataset: { field: 'url' } });
  address._check = () => (!address.value.trim() ? 'Type the indexer address' : address.validity.typeMismatch ? 'A full web address, starting with https' : null);
  const name = h('input', { value: indexer.name ?? '', maxLength: 200, placeholder: 'Name', 'aria-label': 'Indexer name', dataset: { field: 'name' }, spellcheck: false });
  const showVisit = () => {
    const href = webAddress(site.value.trim());
    visit.hidden = !href;
    if (href) visit.href = href; else visit.removeAttribute('href');
    visit.setAttribute('aria-label', `Open the website of ${name.value.trim() || 'this indexer'}`);
  };
  site.addEventListener('input', showVisit);
  name.addEventListener('input', showVisit);
  showVisit();
  const row = h('li', { class: 'indexer' },
    h('div', { class: 'indexer-head' },
      name,
      h('select', { 'aria-label': 'Kind', dataset: { field: 'kind' } },
        INDEXER_KINDS.map((kind) => h('option', { value: kind, selected: (indexer.kind ?? 'json') === kind }, kind))),
      h('input', { type: 'checkbox', class: 'switch', checked: indexer.enabled !== false, 'aria-label': 'Enabled', title: 'Enabled', dataset: { field: 'enabled' } }),
      visit,
      h('button', { type: 'button', class: 'icon-button danger', 'aria-label': 'Remove this indexer', dataset: { act: 'remove' } }, icon('close'))),
    h('div', { class: 'indexer-body' },
      address,
      // Indexers often share one key, so it is replaced here but never removed.
      secretInput(keyName, 'API key', `API key · ${SOURCE_LABELS[secretSource(keyName)].toLowerCase()}`, false),
      site),
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
        check(input);
        value = Math.round(Number(typed) * Number(input.dataset.scale));
      }
    } else {
      value = input.value.trim();
      if (!value && before === undefined) continue;
      check(input);
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
      check(input);
      indexer[input.dataset.field] = input.type === 'checkbox' ? input.checked : input.value.trim();
    }
    readIndexerRules(row, indexer);
    if (!indexer.name) delete indexer.name;
    if (!indexer.site_url) delete indexer.site_url;
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
  const started = access.listen_from_start
    ? `Started with --host or --port, Mynou listens on ${access.listen} until it starts without them${access.local_network_open_from_start ? ', and computers on your local network open it without a password' : ''}. ` : '';
  const state = started + (settings.report.restart_required ? 'Restart Mynou to apply the change.'
    : exposed(access.listen) ? (address ? `From another computer, open ${address}.` : 'Other computers can open Mynou.')
    : 'Only this Mac can open Mynou.');
  const row = (label, hint, control, toggle = false, wide = false) => h('div', { class: `setting-row${toggle ? ' toggle' : ''}${wide ? ' wide' : ''}` },
    h('div', { class: 'setting-label' }, label, hint ? h('p', { class: 'hint' }, hint) : null), control);
  const badge = h('span', { class: 'badge', dataset: { tone: access.password_set ? 'ok' : 'warn' } }, access.password_set ? 'Set' : 'Not set');
  const portInput = h('input', { id: 'network-port', type: 'text', inputMode: 'numeric', value: port, disabled: !mac, spellcheck: false, dataset: { shown: port } });
  portInput._check = () => {
    const typed = portInput.value.trim();
    return typed === portInput.dataset.shown || (/^\d{1,5}$/.test(typed) && Number(typed) >= 1 && Number(typed) <= 65535) ? null : 'A number from 1 to 65535';
  };
  return [
    row(h('label', { htmlFor: 'network-on' }, 'Allow other computers'), state,
      h('input', { id: 'network-on', type: 'checkbox', class: 'switch', checked: on, disabled: !mac, dataset: { shown: String(on) } }), true),
    row(h('label', { htmlFor: 'network-open' }, 'No password on the local network'), 'Computers at home or at the office open Mynou directly. Any other one still needs the password.',
      h('input', { id: 'network-open', type: 'checkbox', class: 'switch', checked: open, disabled: !mac, dataset: { shown: String(open) } }), true),
    row(h('label', { htmlFor: 'network-port' }, 'Port'), 'With 80, addresses need no port.', portInput),
    row(h('label', { htmlFor: 'network-hosts' }, 'Other host names'), `Mynou already answers this Mac's addresses${access.local_name ? ` and ${access.local_name}` : ''}. Add other names, separated by commas.`,
      h('input', { id: 'network-hosts', type: 'text', value: hosts, disabled: !mac, spellcheck: false, placeholder: 'None', dataset: { shown: hosts } }), false, true),
    row(h('label', { htmlFor: 'network-password' }, 'Password', badge), 'At least 10 characters. A new password signs every other computer out.',
      h('span', { class: 'secret' },
        h('input', { id: 'network-password', type: 'password', autocomplete: 'new-password', maxLength: 1024, disabled: !mac, placeholder: access.password_set ? 'Type to replace' : 'Choose a password' }),
        h('button', { type: 'button', class: 'secondary', disabled: !mac, dataset: { networkAct: 'password' } }, 'Save password'),
        access.password_set && (!on || open) && mac ? h('button', { type: 'button', class: 'ghost danger', dataset: { networkAct: 'remove' } }, 'Remove') : null), false, true),
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
    if (!/^\d{1,5}$/.test(port) || Number(port) < 1 || Number(port) > 65535) throw new FieldError($('#network-port'), 'A number from 1 to 65535');
    if (on.checked && !open.checked && !settings.access.password_set) throw new FieldError(on, 'Save a password or turn on No password on the local network, then allow other computers');
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

// ------------------------------------------------------------- unsaved changes --
// The form is read as it would be saved, after every edit, and compared with
// the reading taken when it was painted: the bar shows only real changes, names
// their pages, and leaves once they are undone.

const currentPage = () => (PAGE_TITLES.has(currentParam()) ? currentParam() : SETTING_PAGES[0][0]);

/** The pages whose part of the document differs between two readings. */
function changedPages(before, after) {
  const pages = new Set();
  for (const key of new Set([...Object.keys(before), ...Object.keys(after)])) {
    if (JSON.stringify(before[key]) !== JSON.stringify(after[key])) pages.add(PAGE_OF[key] ?? currentPage());
  }
  return pages;
}

// Controls saved on their own (VPN, friends, the network password) or rebuilt
// as they change (quality) never mark their row by themselves.
const OWN_SAVE = '#settings-vpn, #settings-friends, .quality-defaults, .quality-profiles, #network-password, [type="file"]';
const controlValue = (control) => (control.type === 'checkbox' ? control.checked : control.value);

function startEdits() {
  for (const control of $('#settings-form').querySelectorAll('input, select')) control._initial = controlValue(control);
  try { edits.baseline = readSettingsForm(); } catch { edits.baseline = structuredClone(settings.document); }
  edits.pages = new Set();
  refreshEdits();
}

let refreshQueued = false;
function queueRefresh() {
  if (refreshQueued) return;
  refreshQueued = true;
  requestAnimationFrame(() => { refreshQueued = false; refreshEdits(); });
}

function refreshEdits() {
  if (!edits.baseline) return;
  let pages;
  let problem = null;
  try {
    pages = changedPages(edits.baseline, readSettingsForm());
    edits.pages = pages;
  } catch (error) {
    // A refused value: its page has changes, and a problem without a field
    // of its own is told in the bar.
    pages = new Set(edits.pages);
    const page = pageOf(error.input);
    if (page) pages.add(page);
    if (!error.input) problem = error.message;
  }
  for (const input of $('#settings-form').querySelectorAll('[data-secret]')) if (input.value) pages.add(pageOf(input));
  pages.delete(null);
  edits.dirty = pages.size > 0 || Boolean(problem);
  markRows(pages);
  for (const a of $('#settings-nav').children) setFlag(a, 'data-changed', pages.has(a.dataset.page));
  showProblems();
  paintBar(pages, problem);
}

/** Rows holding a change wear a mark, so a page shows what Save will send. */
function markRows(pages) {
  const form = $('#settings-form');
  const rows = new Set();
  for (const control of form.querySelectorAll('input, select')) {
    if (control.closest(OWN_SAVE)) continue;
    // A control without a painted value belongs to an indexer added since.
    const changed = '_initial' in control ? controlValue(control) !== control._initial : Boolean(control.closest('.indexer'));
    if (changed) rows.add(control.closest('.indexer, .setting-row'));
  }
  const quality = qualityEdits();
  if (quality) {
    for (const key of ['movie_profile', 'episode_profile']) if (quality[key]) rows.add($(`#quality-default-${key}`)?.closest('.setting-row'));
    for (const name of quality.profiles) rows.add(form.querySelector(`.quality-profile[data-profile="${CSS.escape(name)}"]`));
  }
  for (const row of form.querySelectorAll('[data-changed]')) if (!rows.has(row)) row.removeAttribute('data-changed');
  // A value typed back to the saved one in another form ("6.0" for 6) is no change.
  for (const row of rows) if (row) setFlag(row, 'data-changed', pages.has(pageOf(row)));
}

// A problem shows under its field once the field was left, and goes as soon
// as it is fixed, even while typing.
let problemIds = 0;
function setProblem(input, text) {
  if (!input._note?.isConnected) {
    input._note = h('p', { class: 'field-problem', id: `field-problem-${++problemIds}` });
    (input.closest('.with-unit, .secret') ?? input).after(input._note);
    Motion.animate(input._note, [{ opacity: 0, transform: 'translateY(-4px)' }, { opacity: 1, transform: 'none' }], { duration: 220 });
  }
  setText(input._note, text);
  if (input.getAttribute('aria-invalid') !== 'true') {
    input.setAttribute('aria-invalid', 'true');
    input.setAttribute('aria-describedby', input._note.id);
  }
}
function clearProblem(input) {
  if (input.getAttribute('aria-invalid') !== 'true') return;
  input.removeAttribute('aria-invalid');
  input.removeAttribute('aria-describedby');
  input._note?.remove();
  input._note = null;
}
function showProblems() {
  for (const input of $('#settings-form').querySelectorAll('input')) {
    const problem = input._check?.() ?? null;
    if (!problem) clearProblem(input);
    else if (input._touched || input.getAttribute('aria-invalid') === 'true') setProblem(input, problem);
  }
}

// The bar: unsaved changes and where they are, then Saving, then Saved.
const SAVE_KEY = /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘S' : 'Ctrl S';
let barLeaving = null;
function paintBar(pages, problem) {
  const bar = $('#save-bar');
  if (bar.dataset.state) return; // a save in flight owns the bar
  if (!edits.dirty) { closeBar(); return; }
  openBar();
  setText($('#save-title'), 'Unsaved changes');
  const where = $('#save-where');
  setText(where, problem ?? SETTING_PAGES.filter(([id]) => pages.has(id)).map(([, title]) => title).join(' · '));
  setData(where, 'tone', problem ? 'bad' : '');
}
function openBar() {
  const bar = $('#save-bar');
  if (barLeaving) { const leaving = barLeaving; barLeaving = null; leaving.cancel(); }
  if (!bar.hidden) return;
  bar.hidden = false;
  Motion.animate(bar, [{ opacity: 0, transform: 'translateY(18px) scale(.96)' }, { opacity: 1, transform: 'none' }], { duration: 520, easing: Motion.spring() });
}
function closeBar() {
  const bar = $('#save-bar');
  if (bar.hidden || barLeaving) return;
  const leaving = Motion.animate(bar, [{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'translateY(14px) scale(.96)' }], { duration: 220, fill: 'forwards' });
  const done = () => {
    if (leaving && barLeaving !== leaving) return;
    barLeaving = null;
    bar.hidden = true;
    leaving?.cancel();
  };
  barLeaving = leaving;
  if (leaving) leaving.finished.then(done, done); else done();
}
function barSays(title, where = '') {
  setText($('#save-title'), title);
  setText($('#save-where'), where);
  setData($('#save-where'), 'tone', '');
}

/** Saves the form; a refused field is shown where it is, on its own page. */
async function submitSettings() {
  let next;
  try { next = readSettingsForm(); } catch (error) {
    if (!(error instanceof FieldError)) throw error;
    for (const input of $('#settings-form').querySelectorAll('input')) if (input._check?.()) input._touched = true;
    showProblems();
    await bringIntoView(error.input, error.message);
    Motion.nudge($('#save-bar'));
    return;
  }
  const bar = $('#save-bar');
  bar.dataset.state = 'saving';
  barSays('Saving…');
  try {
    for (const [name, value] of typedSecrets()) await saveSecret(name, value);
    await saveSettings(next);
  } catch (error) {
    delete bar.dataset.state;
    refreshEdits();
    throw error;
  }
  bar.dataset.state = 'saved';
  barSays('Saved');
  setTimeout(() => {
    if (bar.dataset.state !== 'saved') return;
    delete bar.dataset.state;
    refreshEdits();
  }, 1100);
}

/** Opens the page of `target`, scrolls it into view and gives it the focus. */
async function bringIntoView(target, problem = null) {
  if (!target) return;
  clearSearch();
  const page = pageOf(target);
  if (page && page !== currentPage()) {
    const arrived = new Promise((resolve) => { addEventListener('hashchange', resolve, { once: true }); setTimeout(resolve, 400); });
    location.hash = `#settings/${page}`;
    await arrived;
  }
  for (let details = target.closest('details'); details; details = details.parentElement.closest('details')) details.open = true;
  if (problem) { target._touched = true; setProblem(target, problem); }
  await new Promise(requestAnimationFrame);
  const row = target.closest('.indexer, .setting-row, .quality-profile') ?? target;
  row.scrollIntoView({ block: 'center', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'instant' : 'smooth' });
  const control = target.matches('input, select, button, summary') ? target
    : [...row.querySelectorAll('input:not([disabled]), select:not([disabled]), button:not([disabled]), summary')].find((el) => el.getClientRects().length);
  control?.focus({ preventScroll: true });
  row.classList.remove('lit');
  void row.offsetWidth;
  row.classList.add('lit');
  row.addEventListener('animationend', () => row.classList.remove('lit'), { once: true });
}

// --------------------------------------------------------------------- search --
// Typing in the search shows every matching setting, from every page, with
// what matched painted in; Enter opens the first one.

const search = { query: '', first: null };
const fold = (text) => text.toLowerCase().normalize('NFD').replace(/\p{M}/gu, '');
const escapeRegExp = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
// Each word of the search starts a word of the setting: "port" finds Port, not Import.
const wordStart = (term, flags = 'u') => new RegExp(`(?<![\\p{L}\\p{N}])${escapeRegExp(term)}`, flags);
const highlights = typeof Highlight === 'function' && CSS.highlights ? CSS.highlights : null;
const UNITS = '.setting-row, #settings-indexers > li, .quality-profile';
// A quality profile is found by its name; its own rows go with it.
const unitsOf = (group) => [...group.querySelectorAll(UNITS)].filter((unit) => !unit.parentElement.closest('.quality-profile'));

/** What a search reads in a setting: its label and hint, its buttons, an indexer's name and address. */
function unitText(unit) {
  if (unit.matches('.indexer')) return [...unit.querySelectorAll('[data-field="name"], [data-field="url"], .indexer-state')].map((el) => el.value ?? el.textContent).join(' ');
  if (unit.matches('.quality-profile')) return unit.querySelector('summary').textContent;
  return [...unit.querySelectorAll('.setting-label, button, output')].map((el) => el.textContent).join(' ');
}

/** Ranges over each word of the text under `root` that a search word starts. */
function matchRanges(root, words, ranges) {
  if (!highlights || !root) return;
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    for (const pattern of words) {
      pattern.lastIndex = 0;
      for (let found = pattern.exec(node.data); found; found = pattern.exec(node.data)) {
        const range = new Range();
        range.setStart(node, found.index);
        range.setEnd(node, found.index + found[0].length);
        ranges.push(range);
      }
    }
  }
}

function applySearch(query) {
  const form = $('#settings-form');
  form.classList.add('searching');
  const terms = fold(query).split(/\s+/).filter(Boolean);
  const tests = terms.map((term) => wordStart(term));
  const words = query.split(/\s+/).filter(Boolean).map((term) => wordStart(term, 'giu'));
  const counts = new Map();
  const ranges = [];
  let first = null;
  let firstGroup = true;
  for (const group of form.querySelectorAll('.setting-group[data-page]')) {
    const page = group.dataset.page;
    const heading = $('.setting-intro h2', group);
    // Every word must be found, in the setting or in the names of its group and page.
    const context = `${PAGE_TITLES.get(page)} ${heading?.textContent ?? ''}`;
    setShown(group, true);
    let hits = 0;
    for (const unit of unitsOf(group)) {
      const text = fold(`${context} ${unitText(unit)}`);
      const hit = tests.every((test) => test.test(text));
      setFlag(unit, 'data-unmatched', !hit);
      if (!hit || unit.closest('[hidden]')) continue;
      hits += 1;
      first ??= unit;
      matchRanges(unit.matches('.quality-profile') ? $('summary', unit) : $('.setting-label', unit), words, ranges);
    }
    if (hits) matchRanges(heading, words, ranges);
    setShown(group, hits > 0);
    group.classList.toggle('first', hits > 0 && firstGroup);
    if (hits) firstGroup = false;
    // Where a result comes from, when its group is not named after its page.
    setData(group, 'from', heading?.textContent === PAGE_TITLES.get(page) ? '' : PAGE_TITLES.get(page));
    counts.set(page, (counts.get(page) ?? 0) + hits);
  }
  if (highlights) highlights.set('settings-match', new Highlight(...ranges));
  for (const a of $('#settings-nav').children) setFlag(a, 'data-dim', !counts.get(a.dataset.page));
  const empty = $('#settings-no-match');
  setText(empty, first ? '' : `No setting matches “${query}”.`);
  setShown(empty, !first);
  search.first = first;
}

function endSearch() {
  const form = $('#settings-form');
  if (!form.classList.contains('searching')) return;
  form.classList.remove('searching');
  for (const unit of form.querySelectorAll('[data-unmatched]')) unit.removeAttribute('data-unmatched');
  for (const a of $('#settings-nav').querySelectorAll('[data-dim]')) a.removeAttribute('data-dim');
  highlights?.delete('settings-match');
  setShown($('#settings-no-match'), false);
  search.first = null;
}

function clearSearch() {
  const input = $('#settings-search');
  if (!input.value && !search.query) return;
  input.value = '';
  search.query = '';
  showPage(true);
}

// ------------------------------------------------------------ navigation states --
// The list of pages tells, at a glance, what needs a look: the tunnel's state,
// Plex or TMDB still to connect, the indexers and whether one fails.

function navState(page, tone, text, label) {
  const slot = $(`#settings-nav [data-page="${page}"] .nav-state`);
  const key = `${tone}\n${text}\n${label}`;
  if (!slot || slot._key === key) return;
  slot._key = key;
  slot.hidden = !tone && !text;
  slot.dataset.tone = tone ?? '';
  slot.dataset.kind = !text ? 'dot' : /^\d+$/.test(text) ? 'count' : 'label';
  slot.title = label ?? '';
  slot.replaceChildren(text, h('span', { class: 'visually-hidden' }, label ? `, ${label}` : ''));
}

function paintNavStates() {
  const vpn = data.get('status')?.data?.vpn;
  const resting = vpn?.state === 'reconnecting' && vpn.reason === 'Exhausted';
  const vpnTone = !vpn?.enabled || vpn.state === 'external' || vpn.state === 'disabled' ? null
    : vpn.state === 'protected' ? 'ok' : resting || ['blocked', 'error'].includes(vpn.state) ? 'bad' : 'warn';
  navState('vpn', vpnTone, '', vpnTone ? (resting ? 'Waiting' : VPN_STATES[vpn.state]?.[0] ?? sentence(vpn.state)) : null);
  if (!settings.report) return;
  const missing = [!plexConnected() && 'Plex is not connected',
    secretSource(String(settingValue('catalog.token_env') ?? '')) === 'missing' && 'The TMDB token is missing'].filter(Boolean);
  navState('services', missing.length ? 'warn' : null, missing.length ? 'Set up' : '', missing.join('; '));
  const count = itemsOf(settings.document.indexers).length;
  const failing = indexerReports().filter((report) => report?.last_error && report.last_error !== 'disabled').length;
  navState('indexers', !count ? 'warn' : failing ? 'bad' : '', String(count),
    !count ? 'No indexer yet' : failing ? `${failing} failing` : `${count} ${count === 1 ? 'indexer' : 'indexers'}`);
}

/** Waits until the service applied a settings change. */
async function applied(reply) {
  if (reply.state === 'applying') {
    settingsState('Applying the new settings…');
    if ($('#save-bar').dataset.state === 'saving') barSays('Applying…', 'Mynou restarts its engine');
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
  setText(badge, state);
  setData(badge, 'tone', tone);
  const age = Number(vpn?.handshake_age_secs);
  const through = profiles.length > 1 && vpn?.active_profile ? `Through ${vpn.active_profile}. ` : '';
  setText($('.hint', row('state')), external ? 'This Mynou uses an outside network; it runs no VPN of its own.'
    : vpn?.state === 'protected' ? `${through}${vpn.ipv6 ? 'IPv4 and IPv6.' : 'IPv4 only.'} ${Number.isFinite(age) && vpn.handshake_age_secs !== null ? `Last handshake ${age} s ago.` : 'Connected.'}`
    : vpn?.state === 'disabled' ? (profile ? 'Everything uses your own connection.' : 'Import a profile to use a VPN.')
    : resting ? `${profiles.length > 1 ? 'Every profile failed' : 'The VPN server never answered'}, so torrents wait. Next try ${relativeTime(Date.now() / 1000 + (Number(vpn.retry_in_secs) || 0))}`
    : vpn?.state === 'connecting' ? `${through}Waiting for the VPN server to answer.`
    : vpn?.reason && vpn.reason !== VPN_RESTARTING ? VPN_REASONS[vpn.reason] ?? vpn.reason : '');
  // A failed tunnel is retried from here: turning it off would send torrents direct.
  const failed = vpn?.enabled && !revoked && (resting || vpn.state === 'error' || (vpn.state === 'blocked' && vpn.reason !== VPN_RESTARTING));
  const enable = $('[data-vpn-act="enable"]', list);
  setShown(enable, !(external || !profile || (vpn.enabled && !revoked && !failed)));
  setText(enable, failed ? 'Try again' : 'Turn on');
  setShown($('[data-vpn-act="disable"]', list), !(external || !vpn?.enabled));
  // The choices only matter while the VPN is on.
  setShown($('#settings-vpn-uses'), !(external || !vpn?.enabled));
  setShown(row('profile'), Boolean(vpn) && !external);
  const imported = $('.badge', row('profile'));
  setText(imported, profiles.length > 1 ? `${profiles.length} profiles` : profile ? 'Imported' : 'Not imported');
  setData(imported, 'tone', profile ? 'ok' : 'warn');
  setText($('.hint', row('profile')), profiles.length > 1
    ? 'Tried in this order. When a server stops answering for about five minutes, the next profile takes over.'
    : 'The WireGuard .conf file from your provider. Add others to fall back on when its server stops answering.');
  setText($('[data-vpn-act="import"]', list), profile ? 'Add a .conf' : 'Import a .conf');
  paintVpnProfiles($('[data-vpn="list"]', list), vpn);
  const switches = itemsOf(vpn?.switches);
  setShown(row('switches'), !external && switches.length > 0);
  const switchList = $('.vpn-switches', list);
  // Relative times move on with the minute; nothing else changes them.
  const switchSignature = JSON.stringify([switches, Math.floor(Date.now() / 60_000)]);
  if (switchList._signature !== switchSignature) {
    switchList._signature = switchSignature;
    switchList.replaceChildren(...switches.map((change) => h('li', {},
      h('span', {}, `${change.from ?? '?'} → ${change.to ?? '?'}`),
      h('small', {}, `${relativeTime(change.at)} · ${VPN_SWITCHES[change.reason] ?? sentence(change.reason)}`))));
  }
  setShown(row('forwarding'), profile && !external);
  const forwarding = $('#vpn-forwarding');
  if (!forwarding.disabled) forwarding.checked = Boolean(vpn?.forwarding) && vpn.forwarding !== 'disabled';
  const hint = $('.hint', row('forwarding'));
  setText(hint, {
    active: `Port ${vpn?.forwarded_port} is open to other peers.`,
    acquiring: 'Asking the VPN server for a port…',
    unavailable: `Unavailable: ${vpn?.forwarding_reason ?? 'the server refused'}.`,
  }[vpn?.forwarding] ?? 'Needs a server with port forwarding (NAT-PMP), such as Proton P2P servers.');
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
  // Opening the page asks Plex for the list when it is old.
  if (currentParam() === 'services' && plex?.connected && !friends.asking
    && Date.now() - checked * 1000 > FRIENDS_STALE_MS && Date.now() - friends.askedAt > FRIENDS_STALE_MS) {
    checkFriends().catch(() => {});
  }
  // A switch waiting for its answer is repainted once the answer is in.
  if (list.querySelector('[data-friend-switch][data-busy]')) return;
  // Rebuilt only when the list, the asking or the minute changed: a push
  // every second never swaps a switch under the pointer.
  const signature = JSON.stringify([requesters ?? null, friends.asking, Math.floor(Date.now() / 60_000)]);
  if (list._signature === signature) return;
  list._signature = signature;
  const count = listed.filter((f) => f.state !== 'left').length;
  const hint = !requesters ? '…'
    : !plex?.connected ? 'Connect Plex above, then your friends show here.'
    : plex.error ? plexError(plex.error)
    : friends.asking && !checked ? 'Asking Plex…'
    : checked ? `${count ? `${count.toLocaleString()} ${count === 1 ? 'friend' : 'friends'}` : 'No friends'} on Plex, checked ${relativeTime(checked)}.`
    : 'Not checked yet.';
  const head = h('div', { class: 'setting-row toggle' },
    h('div', { class: 'setting-label' }, h('span', {}, 'Plex friends'), h('p', { class: `hint${plex?.error ? ' bad' : ''}` }, hint)),
    h('button', { type: 'button', class: 'secondary', disabled: !plex?.connected || friends.asking, dataset: { friendsAct: 'check' } }, icon('sync'), 'Check now'));
  list.replaceChildren(head, ...listed.map(friendRow), ...accounts.map(accountRow));
  if (search.query) showPage(true);
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
// Pushes repaint the view every second; the page is shown again only when
// the page, the search or the groups changed.
let shownKey = '';
function showPage(force = false) {
  const page = currentPage();
  const key = `${page}\n${search.query}`;
  if (!force && key === shownKey) return;
  shownKey = key;
  const nav = $('#settings-nav');
  for (const a of nav.children) {
    if (a.dataset.page !== page) a.removeAttribute('aria-current');
    else if (a.getAttribute('aria-current') !== 'page') {
      a.setAttribute('aria-current', 'page');
      // On a phone the pages scroll sideways: the current one comes to the middle.
      if (nav.scrollWidth > nav.clientWidth) {
        nav.scrollTo({ left: a.offsetLeft - (nav.clientWidth - a.offsetWidth) / 2, behavior: shownKey ? 'smooth' : 'instant' });
      }
    }
  }
  setFlag($('#settings-nav'), 'data-searching', Boolean(search.query));
  if (search.query) { applySearch(search.query); return; }
  endSearch();
  let first = true;
  for (const group of $('#settings-form').querySelectorAll('.setting-group[data-page]')) {
    setShown(group, group.dataset.page === page);
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
    const nav = $('#settings-nav');
    nav.replaceChildren(...SETTING_PAGES.map(([page, title, symbol, starts]) =>
      h('a', { href: `#settings/${page}`, class: starts ? 'starts-set' : undefined, dataset: { page } },
        h('span', { class: 'settings-nav-icon' }, icon(symbol)), h('span', { class: 'settings-nav-label' }, title),
        h('span', { class: 'nav-state', hidden: true }))));
    const form = $('#settings-form');
    // Fields are checked here, under each one, rather than by the browser's bubbles.
    form.noValidate = true;
    form.addEventListener('input', queueRefresh);
    form.addEventListener('change', (event) => {
      event.target._touched = true;
      if (event.target.dataset.path) syncDependents(true);
      queueRefresh();
    });
    const save = $('button[type="submit"]', form);
    $('#save-key').textContent = SAVE_KEY;
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      if (!save.disabled) act(save, submitSettings);
    });
    $('#settings-discard').addEventListener('click', () => this.paint());
    // ⌘S saves from anywhere on the settings; `/` goes to the search.
    document.addEventListener('keydown', (event) => {
      if (currentView() !== 'settings' || document.querySelector('dialog[open]')) return;
      if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 's') {
        event.preventDefault();
        if (edits.dirty && settings.report) form.requestSubmit();
        return;
      }
      if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.target.closest?.('input, textarea, select, [contenteditable]')) return;
      event.preventDefault();
      $('#settings-search').focus();
    });
    // Closing or reloading the tab would lose what is not saved yet.
    addEventListener('beforeunload', (event) => { if (edits.dirty) event.preventDefault(); });
    const find = $('#settings-search');
    find.addEventListener('input', () => { search.query = find.value.trim(); showPage(); });
    find.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') { event.preventDefault(); if (search.first) bringIntoView(search.first); }
      else if (event.key === 'Escape') {
        event.preventDefault();
        if (find.value) clearSearch(); else find.blur();
      }
    });
    // Choosing a page leaves the search for that page.
    nav.addEventListener('click', (event) => {
      const a = event.target.closest('a[data-page]');
      if (!a || !search.query) return;
      find.value = '';
      search.query = '';
      if (a.dataset.page === currentPage()) showPage(true);
    });
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
      Motion.animate(row, [{ opacity: 0, transform: 'translateY(-8px) scale(.98)' }, { opacity: 1, transform: 'none' }], { duration: 420, easing: Motion.spring() });
      $('input', row).focus();
      queueRefresh();
    });
    form.addEventListener('click', (event) => {
      const plex = event.target.closest('[data-plex-act]');
      if (plex) { act(plex, () => plexAction(plex)); return; }
      const server = event.target.closest('[data-plex-server]');
      if (server) { act(server, () => useServer(server.dataset.plexServer)); return; }
      const remove = event.target.closest('#settings-indexers [data-act="remove"]');
      if (remove) {
        const row = remove.closest('li');
        const leaving = Motion.animate(row, [{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'scale(.97)' }], { duration: 180, fill: 'forwards' });
        const gone = () => { row.remove(); queueRefresh(); };
        if (leaving) { row.inert = true; leaving.finished.then(gone, gone); } else gone();
        return;
      }
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
    syncDependents();
    showPage(true);
    this.paintLive();
    startEdits();
    loadPlexSections();
  },
  /** States that change while the page is open; inputs are never touched. */
  paintLive() {
    paintVpn();
    paintFriends();
    paintNavStates();
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
      setShown(state, Boolean(text));
      setText(state, text);
      setData(state, 'tone', tone);
    }
    const status = data.get('status')?.data;
    const hint = $('[data-live="watchlist"]');
    if (!hint || !status) return;
    const accounts = data.get('requesters')?.data?.accounts?.length ?? 0;
    setText(hint, status.last_sync_error ? `Last sync failed: ${plexError(status.last_sync_error)}`
      : accounts ? `${accounts} ${accounts === 1 ? 'watchlist is' : 'watchlists are'} read every minute.` : 'Your watchlist is read every minute.');
    hint.classList.toggle('bad', Boolean(status.last_sync_error));
  },
};
