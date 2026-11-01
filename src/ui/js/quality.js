import { h } from './dom.js';
import { toast } from './components.js';
import { link } from './link.js';

// Quality is chosen with a few plain choices. Mynou turns them into one
// selection profile per kind and quality, named `movie-1080p`, `series-2160p`
// and so on, so that a film or a series can also use another quality than the
// default one. Profiles written by hand under other names are kept as they are.
export const PRESETS = [
  ['2160p', '4K when available', [2160, 1080]],
  ['1080p', 'Up to 1080p', [1080, 720]],
  ['720p', 'Up to 720p, smaller files', [720, 576, 480]],
  ['any', 'Any quality', []],
];
const KINDS = [['movie', 'Films', 'movie_profile', 'per film'], ['series', 'Series', 'episode_profile', 'per episode']];
// Most preferred first. Cam and telesync copies are never wanted.
const SOURCES = ['bluray', 'web-dl', 'webrip', 'remux', 'hdtv', 'dvd'];
const CODECS = ['h265', 'h264', 'av1', 'vp9'];
export const LANGUAGES = [
  ['', 'Any language'], ['en', 'English'], ['fr', 'French'], ['de', 'German'], ['es', 'Spanish'], ['it', 'Italian'],
  ['pt', 'Portuguese'], ['ja', 'Japanese'], ['ko', 'Korean'], ['zh', 'Chinese'], ['ru', 'Russian'],
];
const GB = 1000; // profile sizes are in MB
const PRESET_NAME = /^(movie|series)-(2160p|1080p|720p|any)$/;

const words = (text) => [...new Set(String(text).split(',').map((w) => w.trim().toLowerCase()).filter(Boolean))];

/** What the Quality page shows for one kind, read from its default profile. */
function current(selection, kind, key) {
  const name = selection?.[key];
  const match = PRESET_NAME.exec(name ?? '');
  const profile = selection?.profiles?.[name] ?? {};
  const language = profile.languages?.[0] ?? '';
  return {
    preset: match && match[1] === kind ? match[2] : selection ? 'custom' : 'any',
    language,
    only: Boolean(language) && profile.allow_unknown_language === false,
    min: profile.min_size_mb ?? null,
    max: profile.max_size_mb ?? null,
  };
}

function profile([, , resolutions], kind) {
  const languages = kind.language ? [kind.language] : [];
  return {
    resolutions,
    cutoff_resolution: resolutions[0] ?? null,
    sources: SOURCES,
    codecs: CODECS,
    languages,
    allow_unknown_resolution: !resolutions.length,
    allow_unknown_source: true,
    allow_unknown_codec: true,
    allow_unknown_language: !kind.only,
    min_size_mb: kind.min,
    max_size_mb: kind.max,
    allow_unknown_size: true,
    required_terms: [],
    blocked_terms: kind.avoid,
    score_rules: kind.prefer.map((term) => ({ terms: [term], score: 10 })),
    minimum_score: 0,
  };
}

const sizeText = (mb) => (mb == null ? '' : String(Math.round((mb / GB) * 100) / 100));

/** The Quality page as [title, about, rows] groups; `row` lays out one setting. */
export function qualityGroups(document, row) {
  const selection = document.selection;
  // Words to avoid and prefer are shared: any generated profile holds them.
  const anyProfile = Object.entries(selection?.profiles ?? {}).find(([name]) => PRESET_NAME.test(name))?.[1];
  const groups = [];
  for (const [kind, title, key, per] of KINDS) {
    const now = current(selection, kind, key);
    const presets = now.preset === 'custom' ? [['custom', 'Custom, set through the API', []], ...PRESETS] : PRESETS;
    const id = (field) => `quality-${kind}-${field}`;
    const field = (name, shown) => ({ quality: kind, field: name, shown: String(shown) });
    groups.push([title, `The release Mynou picks for ${kind === 'movie' ? 'a film' : 'an episode or a season pack'}.`, [
      row(id('preset'), 'Quality', 'Mynou picks the best release it finds within this limit.',
        h('select', { id: id('preset'), dataset: field('preset', now.preset) },
          presets.map(([value, label]) => h('option', { value, selected: value === now.preset }, label)))),
      row(id('language'), 'Audio language', 'Releases marked MULTI do not say which languages they hold, so they count as unknown.',
        h('select', { id: id('language'), dataset: field('language', now.language) },
          LANGUAGES.map(([value, label]) => h('option', { value, selected: value === now.language }, label)))),
      row(id('only'), 'Only this language', 'Off: releases in this language come first, others when there is none.',
        h('input', { id: id('only'), type: 'checkbox', class: 'switch', checked: now.only, dataset: field('only', now.only) }), true),
      row(id('min'), 'Smallest size', `Skips fakes and poor encodes, ${per}.`,
        h('span', { class: 'with-unit' }, h('input', { id: id('min'), type: 'text', inputMode: 'decimal', placeholder: 'No minimum', value: sizeText(now.min), dataset: field('min', sizeText(now.min)) }),
          h('span', { class: 'unit', 'aria-hidden': 'true' }, 'GB'))),
      row(id('max'), 'Largest size', `Leaves out huge releases, ${per}.`,
        h('span', { class: 'with-unit' }, h('input', { id: id('max'), type: 'text', inputMode: 'decimal', placeholder: 'No maximum', value: sizeText(now.max), dataset: field('max', sizeText(now.max)) }),
          h('span', { class: 'unit', 'aria-hidden': 'true' }, 'GB'))),
    ]]);
  }
  const avoid = (anyProfile?.blocked_terms ?? []).join(', ');
  const prefer = (anyProfile?.score_rules ?? []).flatMap((rule) => rule.terms ?? []).join(', ');
  groups.push(['Release names', 'Words in release names, for films and series alike.', [
    row('quality-avoid', 'Avoid', 'Words that rule a release out, separated by commas, for example hdcam, 3d.',
      h('input', { id: 'quality-avoid', type: 'text', spellcheck: false, placeholder: 'Nothing', value: avoid, dataset: { quality: 'all', field: 'avoid', shown: avoid } })),
    row('quality-prefer', 'Prefer', 'Words that put a release first, for example proper, repack.',
      h('input', { id: 'quality-prefer', type: 'text', spellcheck: false, placeholder: 'Nothing', value: prefer, dataset: { quality: 'all', field: 'prefer', shown: prefer } })),
  ]]);
  return groups;
}

function size(input, label) {
  const typed = input.value.trim().replace(',', '.');
  if (!typed) return null;
  const mb = Math.round(Number(typed) * GB);
  if (!Number.isFinite(mb) || mb < 1 || mb > 1_000_000) throw new Error(`${label} needs a size from 0.001 to 1000 GB`);
  return mb;
}

/** Folds the Quality page into the settings document, when it changed. */
export function readQuality(document, form) {
  const inputs = [...form.querySelectorAll('[data-quality]')];
  if (!inputs.length) return;
  const value = (input) => (input.type === 'checkbox' ? String(input.checked) : input.value.trim());
  if (inputs.every((input) => value(input) === input.dataset.shown)) return;
  const get = (kind, field) => inputs.find((i) => i.dataset.quality === kind && i.dataset.field === field);
  const avoid = words(get('all', 'avoid').value);
  const prefer = words(get('all', 'prefer').value);
  for (const term of [...avoid, ...prefer]) {
    if (new TextEncoder().encode(term).length > 128) throw new Error('Each word or phrase holds at most 128 bytes');
  }
  if (avoid.length > 32 || prefer.length > 32) throw new Error('Up to 32 words to avoid and 32 to prefer');
  const selection = structuredClone(document.selection ?? { profiles: { any: {} } });
  selection.profiles ??= {};
  for (const [kind, title, key] of KINDS) {
    const choice = {
      language: get(kind, 'language').value,
      only: get(kind, 'only').checked,
      min: size(get(kind, 'min'), `${title}: smallest size`),
      max: size(get(kind, 'max'), `${title}: largest size`),
      avoid,
      prefer,
    };
    if (choice.min != null && choice.max != null && choice.min > choice.max) throw new Error(`${title}: the smallest size is above the largest`);
    for (const preset of PRESETS) selection.profiles[`${kind}-${preset[0]}`] = profile(preset, choice);
    const preset = get(kind, 'preset').value;
    if (preset !== 'custom') selection[key] = `${kind}-${preset}`;
  }
  document.selection = selection;
}

// A film or a series can use another quality than the default one.
let bindings = null;

async function binding(kind, tmdbId) {
  if (!bindings) {
    bindings = new Map();
    let after = null;
    for (let page = 0; page < 20; page++) {
      const reply = await link.call('GET', `/api/title-profiles?limit=200${after ? `&after=${encodeURIComponent(after)}` : ''}`);
      for (const b of reply.bindings ?? []) bindings.set(`${b.kind}:${b.tmdb_id}`, b.profile);
      after = reply.next;
      if (!after) break;
    }
  }
  return bindings.get(`${kind}:${tmdbId}`) ?? null;
}

async function bind(kind, tmdbId, profile) {
  const body = profile ? { action: 'set', kind, tmdb_id: tmdbId, profile } : { action: 'clear', kind, tmdb_id: tmdbId };
  const plan = await link.call('POST', '/api/title-profiles/control', body);
  await link.call('POST', '/api/title-profiles/control', { ...body, apply: true, plan_id: plan.plan_id });
  if (profile) bindings?.set(`${kind}:${tmdbId}`, profile); else bindings?.delete(`${kind}:${tmdbId}`);
}

/** The quality menu of a film (`movie`) or series (`series`) sheet. */
export function titleQuality(kind, tmdbId) {
  const id = Number(tmdbId);
  if (!Number.isSafeInteger(id) || id < 1) return null;
  const select = h('select', { 'aria-label': 'Quality for this title', disabled: true }, h('option', {}, 'Loading…'));
  const hint = h('p', { class: 'hint' }, 'Applies to the next searches and upgrade checks; what is already downloaded stays.');
  Promise.all([link.call('GET', '/api/profiles'), binding(kind, id)]).then(([selection, bound]) => {
    const names = new Set(Object.keys(selection.profiles ?? {}));
    const offered = PRESETS.filter(([preset]) => names.has(`${kind}-${preset}`));
    if (!offered.length) {
      select.replaceChildren(h('option', {}, 'Default'));
      hint.textContent = 'Save your choices in Settings, Quality first.';
      return;
    }
    const custom = bound && !PRESET_NAME.test(bound) ? [h('option', { value: bound, selected: true }, `Profile ${bound}`)] : [];
    select.replaceChildren(h('option', { value: '', selected: !bound }, 'Default, as in Settings'), ...custom,
      ...offered.map(([preset, label]) => h('option', { value: `${kind}-${preset}`, selected: bound === `${kind}-${preset}` }, label)));
    select.disabled = false;
    select.dataset.shown = select.value;
  }).catch((error) => { hint.textContent = error.message; hint.classList.add('bad'); });
  select.addEventListener('change', () => {
    select.disabled = true;
    bind(kind, id, select.value || null)
      .then(() => { select.dataset.shown = select.value; toast('Quality saved for this title'); })
      .catch((error) => { select.value = select.dataset.shown ?? ''; toast(error.message, 'bad'); })
      .finally(() => { select.disabled = false; });
  });
  return h('div', { class: 'setting-list' },
    h('div', { class: 'setting-row' }, h('div', { class: 'setting-label' }, h('span', {}, 'Quality'), hint), select));
}
