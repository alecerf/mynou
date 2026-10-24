import { sentence } from './format.js';
import { $, h, icon } from './dom.js';
import { toast, act, skeleton } from './components.js';
import { Motion } from './motion.js';
import { link } from './link.js';
import { data, views, itemsOf } from './store.js';
import { INDEXER_ERRORS, plexError, indexerReports } from './sources.js';

// The settings document keeps every field; the groups below edit the common
// ones in place and leave the others untouched. Saving sends the whole document,
// which the service validates and applies by restarting its engine.
// A group is [title, description, fields]; a field is [path, label, type, options]: `hint` explains it, `when` names the
// switch it depends on, `scale` and `unit` show a stored number in friendlier
// units, and `zero` is what an empty field means.
const MB = 1_000_000;
const SETTING_GROUPS = [
  ['Library', 'Where finished films and series are filed.', [
    ['library.movies_root', 'Movies folder', 'text'],
    ['library.series_root', 'Series folder', 'text'],
  ]],
  ['Downloads', 'How the built-in client fetches releases.', [
    ['downloads.data_dir', 'Download folder', 'text'],
    ['downloads.max_active', 'Downloads at once', 'number'],
    ['downloads.download_limit_bps', 'Download speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
    ['downloads.upload_limit_bps', 'Upload speed limit', 'number', { scale: MB, unit: 'MB/s', zero: 'No limit' }],
  ]],
  ['Plex', 'Your server and its watchlist.', [
    ['plex.url', 'Server address', 'text'],
    ['plex.token_env', 'Token', 'secret'],
    ['plex.movies_section', 'Movies section', 'text', { hint: 'The library number shown in Plex.' }],
    ['plex.series_section', 'Series section', 'text'],
    ['plex.watchlist', 'Watchlist', 'sync'],
  ]],
  ['TMDB', 'Titles, years and episode lists.', [
    ['catalog.token_env', 'Read access token', 'secret'],
    ['catalog.api_key_env', 'Or API key', 'secret'],
  ]],
  ['Automation', 'What Mynou does on its own.', [
    ['monitoring.enabled', 'Look for better versions', 'check', { hint: 'Replaces a film or episode when a better release appears.' }],
    ['monitoring.interval_secs', 'Check every', 'number', { when: 'monitoring.enabled', scale: 60, unit: 'min' }],
    ['series_packs.enabled', 'Prefer season packs', 'check'],
    ['updates.automatic', 'Update Mynou automatically', 'check', { hint: 'Checked once a day.' }],
  ]],
];
const INDEXER_KINDS = ['torznab', 'json', 'rss'];
const settings = { report: null, document: null, secrets: [], loading: false };

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
  for (const [path, on] of [['plex.enabled', present('plex.token_env')], ['catalog.enabled', present('catalog.token_env') || present('catalog.api_key_env')]]) {
    if (on !== Boolean(settingValue(path))) setPath(document, path, on);
  }
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
  const [report, secrets] = await Promise.all([link.call('GET', '/api/settings'), link.call('GET', '/api/secrets')]);
  settings.report = report;
  settings.document = report.settings;
  settings.secrets = itemsOf(secrets.secrets);
  views.settings.paint();
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

async function saveSecret(name, value) {
  const reply = await link.call('POST', '/api/secrets', { name, value });
  settings.secrets = itemsOf(reply.secrets);
}

views.settings = {
  init() {
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
    $('#settings-groups').replaceChildren(...SETTING_GROUPS.map(([title, about, fields]) =>
      h('section', { class: 'setting-group' },
        h('div', { class: 'setting-intro' }, h('h2', {}, title), h('p', { class: 'hint' }, about)),
        h('div', { class: 'setting-list' }, fields.map(settingRow)))));
    // Saved indexers keep their position, which is also the order of the live reports.
    $('#settings-indexers').replaceChildren(...itemsOf(settings.document.indexers).map((indexer, i) => Object.assign(indexerRow(indexer), { _saved: i })));
    $('#save-bar').hidden = true;
    syncDependents();
    this.paintLive();
  },
  /** States that change while the page is open; inputs are never touched. */
  paintLive() {
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
