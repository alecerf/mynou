import { h, icon } from './dom.js';
import { act, toast } from './components.js';
import { link } from './link.js';

// Quality profiles, as in Radarr and Sonarr but in plain words: which
// qualities a release may have, best first, where upgrades stop, the
// languages, a size per hour of video, and a few named preferences. Films and
// series share the profiles; each kind has its default, and each title can
// use another one, chosen when it is added or later from its sheet.

export const RESOLUTIONS = [[2160, '4K'], [1080, '1080p'], [720, '720p'], [576, '576p'], [480, '480p']];
// Best first; cam and telesync copies are never offered.
const SOURCES = [['remux', 'Remux'], ['bluray', 'Blu-ray'], ['web-dl', 'WEB-DL'], ['webrip', 'WEBRip'], ['hdtv', 'HDTV'], ['dvd', 'DVD']];
const CODECS = ['h265', 'h264', 'av1', 'vp9'];
export const LANGUAGES = [
  ['en', 'English'], ['fr', 'French'], ['de', 'German'], ['es', 'Spanish'], ['it', 'Italian'],
  ['pt', 'Portuguese'], ['ja', 'Japanese'], ['ko', 'Korean'], ['zh', 'Chinese'], ['ru', 'Russian'],
];
// Languages a profile can ask for, as release names tag them. French releases
// name their dub; "French" alone stands for any of them, or for FRENCH and VF.
const AUDIO = [
  ['vff', 'VFF', 'France dub, also TRUEFRENCH'],
  ['vfq', 'VFQ', 'Québec dub'],
  ['vfi', 'VFI', 'International dub'],
  ['fr', 'French', 'Any French, dub named or not'],
  ['vostfr', 'VOSTFR', 'Original audio, French subtitles; MULTi with French counts'],
  // Resolved for each title from TMDB.
  ['original', 'Original language', "Each title's own, from TMDB: VO, MULTi or VOSTFR; French audio for a French title"],
  ...LANGUAGES.filter(([value]) => value !== 'fr').map(([value, label]) => [value, label, null]),
];
const FRENCH = new Set(['vff', 'vfq', 'vfi', 'fr', 'vostfr']);
const audioGroup = (key) => (key === 'original' ? 'title' : FRENCH.has(key) ? 'french' : 'other');
const audioName = (value) => AUDIO.find(([key]) => key === value)?.[1] ?? languageName(value);
/** What a list of profile languages reads as: "VFF and VOSTFR", all of them needed. */
const languagesName = (languages) => {
  const names = languages.map((value, index) => (index && value === 'original' ? 'original language' : audioName(value)));
  return names.length > 1 ? `${names.slice(0, -1).join(', ')} and ${names.at(-1)}` : names[0];
};
// MULTI holds the original audio too; it is set from the audio rows.
const MULTI = [['multi']];
// Named preferences: each one stands for words in release names.
const EXTRAS = [
  ['hdr', 'Prefer HDR', 'prefer', [['hdr']]],
  ['dv', 'Prefer Dolby Vision', 'prefer', [['dv'], ['dovi']]],
  ['3d', 'Avoid 3D', 'avoid', [['3d']]],
];
const EXTRA_TERMS = new Set([...EXTRAS.flatMap(([, , , rules]) => rules.flat()), ...MULTI.flat()]);
const GB = 1000; // profile sizes are in MB
const PREFER_SCORE = 10;

const words = (text) => [...new Set(String(text).split(',').map((w) => w.trim().toLowerCase()).filter(Boolean))];
const named = (pairs, value) => pairs.find(([key]) => key === value)?.[1] ?? String(value);
const resolutionName = (value) => named(RESOLUTIONS, value);
const sourceName = (value) => named(SOURCES, value);
const languageName = (value) => named(LANGUAGES, value);
export const profileLabel = (name, profile) => profile?.label || name;

/** The language mode a profile works with, as the server derives it for older profiles. */
function languageMode(profile) {
  if (profile.language_mode) return profile.language_mode;
  if (!profile.languages?.length) return 'any';
  return profile.allow_unknown_language === false ? 'require' : 'prefer';
}

/** One line that says what a profile does, from its real settings. */
export function profileSummary(profile) {
  const res = profile.resolutions ?? [];
  const parts = [];
  if (!res.length) parts.push('Any quality');
  else parts.push(res.length === 1 ? `${resolutionName(res[0])} only` : `${resolutionName(res[0])}, ${res.slice(1).map(resolutionName).join(', ')} accepted`);
  if (profile.cutoff_resolution) {
    parts.push(`upgrades until ${resolutionName(profile.cutoff_resolution)}${profile.cutoff_source ? ` ${sourceName(profile.cutoff_source)}` : ''}`);
  }
  const mode = languageMode(profile);
  const languages = profile.languages ?? [];
  if (mode !== 'any' && languages.length) {
    const name = languagesName(languages);
    parts.push(mode === 'require' ? `${name} required` : `${name} first`);
  }
  if (hasExtra(profile, 'prefer', MULTI)) parts.push('MULTI first');
  if (profile.max_mb_per_hour) parts.push(`up to ${Math.round((profile.max_mb_per_hour / GB) * 10) / 10} GB per hour`);
  // "Prefer HDR" reads "prefer HDR" mid-sentence; acronyms keep their case.
  for (const [, label, kind, rules] of EXTRAS) if (hasExtra(profile, kind, rules)) parts.push(label[0].toLowerCase() + label.slice(1));
  return parts.join(' · ');
}

function hasExtra(profile, kind, rules) {
  if (kind === 'avoid') return rules.every(([term]) => (profile.blocked_terms ?? []).includes(term));
  return rules.every(([term]) => (profile.score_rules ?? []).some((rule) => rule.terms?.length === 1 && rule.terms[0] === term && rule.score > 0));
}

function setExtra(profile, kind, rules, on) {
  if (kind === 'avoid') {
    const terms = new Set(profile.blocked_terms ?? []);
    for (const [term] of rules) if (on) terms.add(term); else terms.delete(term);
    profile.blocked_terms = [...terms];
    return;
  }
  const kept = (profile.score_rules ?? []).filter((rule) => !(rule.terms?.length === 1 && rules.some(([term]) => term === rule.terms[0])));
  profile.score_rules = on ? [...kept, ...rules.map((terms) => ({ terms, score: PREFER_SCORE }))] : kept;
}

/** Words the operator typed, apart from the named preferences. */
const preferWords = (profile) => (profile.score_rules ?? [])
  .filter((rule) => rule.score > 0 && !(rule.terms?.length === 1 && EXTRA_TERMS.has(rule.terms[0])))
  .map((rule) => rule.terms.join(' '));
const avoidWords = (profile) => (profile.blocked_terms ?? []).filter((term) => !EXTRA_TERMS.has(term));

/** A fresh profile, a copy of `from` when given. */
function newProfile(from) {
  const profile = structuredClone(from ?? {
    resolutions: [1080, 720], cutoff_resolution: 1080, cutoff_source: 'bluray',
    sources: SOURCES.slice(1).map(([key]) => key), codecs: CODECS, languages: [], language_mode: 'any',
    allow_unknown_resolution: false, allow_unknown_source: true, allow_unknown_codec: true, allow_unknown_language: true,
    required_terms: [], blocked_terms: ['3d'], score_rules: [], minimum_score: 0, allow_unknown_size: true,
  });
  profile.label = from ? `${profileLabel('', from) || 'Profile'} copy` : 'New profile';
  return profile;
}

/** A unique profile key from a label: letters, digits and hyphens. */
function keyFor(label, taken) {
  const base = (label.toLowerCase().normalize('NFD').replace(/\p{M}/gu, '').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'profile').slice(0, 56);
  let key = base;
  for (let n = 2; taken.has(key); n++) key = `${base}-${n}`;
  return key;
}

// The page edits a copy of the saved selection; Save sends it with the other settings.
let draft = null;
let saved = '';
let savedDraft = null;

/** What differs from the saved selection: each default, and the profiles changed or added. */
export function qualityEdits() {
  if (!draft) return null;
  const json = (value) => JSON.stringify(value ?? null);
  return {
    movie_profile: draft.movie_profile !== savedDraft.movie_profile,
    episode_profile: draft.episode_profile !== savedDraft.episode_profile,
    profiles: new Set(Object.keys(draft.profiles).filter((name) => json(draft.profiles[name]) !== json(savedDraft.profiles?.[name]))),
  };
}

/** The Quality page as [title, about, rows] groups; `row` lays out one setting. */
export function qualityGroups(document, row) {
  draft = structuredClone(document.selection ?? { profiles: { any: {} }, movie_profile: 'any', episode_profile: 'any' });
  draft.profiles ??= {};
  saved = JSON.stringify(draft);
  savedDraft = JSON.parse(saved);
  const container = h('div', { class: 'quality-profiles' });
  const changed = () => container.dispatchEvent(new Event('input', { bubbles: true }));
  const defaults = h('div', { class: 'quality-defaults' });
  const paint = (open) => {
    defaults.replaceChildren(
      defaultRow(row, 'movie_profile', 'Films', changed, paint),
      defaultRow(row, 'episode_profile', 'Series', changed, paint));
    container.replaceChildren(...Object.keys(draft.profiles).map((name) => profileCard(name, row, changed, paint, name === open)),
      h('div', { class: 'actions' }, h('button', {
        type: 'button', class: 'secondary',
        onclick: () => {
          const name = keyFor('New profile', new Set(Object.keys(draft.profiles)));
          draft.profiles[name] = newProfile(draft.profiles[draft.movie_profile]);
          draft.profiles[name].label = 'New profile';
          changed();
          paint(name);
        },
      }, icon('requests'), 'Create profile')));
  };
  paint(null);
  return [
    ['Defaults', 'The profile a new film or series uses. You can pick another one when you add a title, or later from its page.', [defaults]],
    ['Profiles', 'What a release must be, best first. Mynou picks the best release a profile accepts, then upgrades it until the level you set.', [container]],
  ];
}

function defaultRow(row, key, title, changed, paint) {
  const select = h('select', { id: `quality-default-${key}` },
    Object.entries(draft.profiles).map(([name, profile]) => h('option', { value: name, selected: name === draft[key] }, profileLabel(name, profile))));
  select.addEventListener('change', () => { draft[key] = select.value; changed(); paint(null); });
  return row(select.id, title, profileSummary(draft.profiles[draft[key]] ?? {}), select);
}

function profileCard(name, row, changed, paint, open) {
  const profile = draft.profiles[name];
  const summary = h('span', { class: 'hint' }, profileSummary(profile));
  const title = h('strong', {}, profileLabel(name, profile));
  const tags = [name === draft.movie_profile ? 'Films' : null, name === draft.episode_profile ? 'Series' : null].filter(Boolean);
  // Controls that show other settings of the card repaint after each change.
  const hooks = [];
  const update = () => {
    summary.textContent = profileSummary(profile);
    title.textContent = profileLabel(name, profile);
    for (const hook of hooks) hook();
    changed();
  };
  const id = (field) => `quality-${name}-${field}`;
  const body = h('div', { class: 'setting-list' },
    row(id('label'), 'Name', null, textInput(id('label'), profile.label ?? name, 64, (value) => { profile.label = value || undefined; update(); })),
    row(id('res'), 'Qualities', 'Ticked ones are accepted. The first one Mynou finds in this order wins; a higher source wins within the same quality.', qualityLadder(profile, update)),
    row(id('src'), 'Sources', 'None ticked: any source.', sourceChoices(profile, update)),
    row(id('cutoff'), 'Upgrade until', 'Once a file reaches this, Mynou stops looking for better ones.', cutoffChoice(id('cutoff'), profile, update, hooks)),
    ...audioRows(id, row, profile, update, hooks),
    row(id('max'), 'Largest size', 'Per hour of video, from the length TMDB gives. Empty: no limit.', sizeInput(id('max'), profile, 'max_mb_per_hour', update)),
    row(id('min'), 'Smallest size', 'Per hour of video. Skips fakes and poor encodes.', sizeInput(id('min'), profile, 'min_mb_per_hour', update)),
    row(id('unknown'), 'Unknown sizes', 'Off: a release that does not say its size is skipped when a size limit is set.',
      check(id('unknown'), profile.allow_unknown_size !== false, (on) => { profile.allow_unknown_size = on; update(); }), true),
    row(id('extras'), 'Preferences', 'Proper and repack releases are always preferred over the first release of the same quality.', extraChoices(profile, update)),
    row(id('prefer'), 'Prefer words', 'Words in release names that put a release first within the same quality, separated by commas.',
      textInput(id('prefer'), preferWords(profile).join(', '), 1024, (value) => { setWords(profile, 'prefer', words(value)); update(); })),
    row(id('avoid'), 'Avoid words', 'Words that rule a release out, separated by commas.',
      textInput(id('avoid'), avoidWords(profile).join(', '), 1024, (value) => { setWords(profile, 'avoid', words(value)); update(); })),
    row(id('require'), 'Required words', 'A release must hold every one of them. Empty: none.',
      textInput(id('require'), (profile.required_terms ?? []).join(', '), 1024, (value) => { profile.required_terms = words(value); update(); })),
    tryOut(name, profile, hooks),
    h('div', { class: 'actions' },
      h('button', {
        type: 'button', class: 'secondary',
        onclick: () => { const key = keyFor(`${profileLabel(name, profile)} copy`, new Set(Object.keys(draft.profiles))); draft.profiles[key] = newProfile(profile); changed(); paint(key); },
      }, 'Duplicate'),
      h('button', {
        type: 'button', class: 'ghost danger', disabled: tags.length > 0 || Object.keys(draft.profiles).length < 2,
        title: tags.length ? 'A default profile cannot be deleted' : undefined,
        onclick: () => { delete draft.profiles[name]; changed(); paint(null); },
      }, 'Delete')));
  return h('details', { class: 'quality-profile', open, dataset: { profile: name } },
    h('summary', {}, h('span', { class: 'quality-profile-head' }, title, ...tags.map((tag) => h('span', { class: 'badge', dataset: { tone: 'accent' } }, tag))), summary),
    body);
}

function textInput(id, value, max, set) {
  return h('input', { id, type: 'text', spellcheck: false, maxLength: max, value, oninput: (event) => set(event.target.value.trim()) });
}
function check(id, on, set) {
  return h('input', { id, type: 'checkbox', class: 'switch', checked: on, onchange: (event) => set(event.target.checked) });
}

/** The accepted qualities, best first, with arrows to reorder them. */
function qualityLadder(profile, update) {
  const list = h('ol', { class: 'quality-ladder' });
  const order = () => [...(profile.resolutions ?? []), ...RESOLUTIONS.map(([value]) => value).filter((value) => !(profile.resolutions ?? []).includes(value))];
  let shown = order();
  const paint = () => {
    list.replaceChildren(...shown.map((value, index) => {
      const on = (profile.resolutions ?? []).includes(value);
      const move = (step) => {
        const to = index + step;
        if (to < 0 || to >= shown.length) return;
        [shown[index], shown[to]] = [shown[to], shown[index]];
        save();
      };
      return h('li', { class: on ? 'on' : '' },
        h('label', {}, h('input', { type: 'checkbox', checked: on, onchange: (event) => { event.stopPropagation(); toggle(value, event.target.checked); } }), resolutionName(value)),
        h('span', { class: 'ladder-moves' },
          h('button', { type: 'button', class: 'icon-button', 'aria-label': `Move ${resolutionName(value)} up`, disabled: index === 0, onclick: () => move(-1) }, '↑'),
          h('button', { type: 'button', class: 'icon-button', 'aria-label': `Move ${resolutionName(value)} down`, disabled: index === shown.length - 1, onclick: () => move(1) }, '↓')));
    }));
  };
  const toggle = (value, on) => {
    const accepted = new Set(profile.resolutions ?? []);
    if (on) accepted.add(value); else accepted.delete(value);
    profile.resolutions = shown.filter((v) => accepted.has(v));
    save();
  };
  const save = () => {
    profile.resolutions = shown.filter((v) => (profile.resolutions ?? []).includes(v));
    // An empty list accepts any quality, including releases that do not say theirs.
    profile.allow_unknown_resolution = !profile.resolutions.length;
    if (profile.cutoff_resolution && !profile.resolutions.includes(profile.cutoff_resolution)) {
      profile.cutoff_resolution = profile.resolutions[0] ?? null;
    }
    paint();
    update();
  };
  paint();
  return list;
}

function sourceChoices(profile, update) {
  return h('div', { class: 'choice-chips' }, SOURCES.map(([key, label]) => h('label', { class: 'chip' },
    h('input', {
      type: 'checkbox', checked: (profile.sources ?? []).includes(key),
      onchange: (event) => {
        const chosen = new Set(profile.sources ?? []);
        if (event.target.checked) chosen.add(key); else chosen.delete(key);
        profile.sources = SOURCES.map(([k]) => k).filter((k) => chosen.has(k));
        profile.allow_unknown_source = true;
        if (profile.cutoff_source && !chosen.has(profile.cutoff_source)) profile.cutoff_source = null;
        update();
      },
    }), label)));
}

/** Where upgrades stop: a quality and, optionally, a source. */
function cutoffChoice(id, profile, update, hooks) {
  const resolution = h('select', { id, 'aria-label': 'Upgrade until quality' });
  const source = h('select', { 'aria-label': 'Upgrade until source' });
  const paint = () => {
    const accepted = profile.resolutions ?? [];
    resolution.replaceChildren(h('option', { value: '', selected: !profile.cutoff_resolution }, accepted.length ? 'Always look for better' : 'Any quality is final'),
      ...accepted.map((value) => h('option', { value, selected: value === profile.cutoff_resolution }, resolutionName(value))));
    resolution.disabled = !accepted.length;
    const sources = (profile.sources?.length ? profile.sources : SOURCES.map(([key]) => key));
    source.replaceChildren(h('option', { value: '', selected: !profile.cutoff_source }, 'any source'),
      ...sources.map((key) => h('option', { value: key, selected: key === profile.cutoff_source }, sourceName(key))));
    source.hidden = !profile.cutoff_resolution;
  };
  resolution.addEventListener('change', () => {
    profile.cutoff_resolution = Number(resolution.value) || null;
    if (!profile.cutoff_resolution) profile.cutoff_source = null;
    update();
  });
  source.addEventListener('change', () => { profile.cutoff_source = source.value || null; update(); });
  // The ladder and the sources change what can be chosen here.
  hooks.push(paint);
  paint();
  return h('span', { class: 'cutoff-choice' }, resolution, source);
}

/** The languages, in a few plain rows: those a release must have, then what
    happens to a release missing some. Rows that do not apply stay hidden. */
function audioRows(id, row, profile, update, hooks) {
  const chosen = () => profile.languages ?? [];
  const list = h('ol', { class: 'audio-list' });
  const add = h('select', { id: id('lang'), class: 'audio-add', 'aria-label': 'Add a language' });
  const set = (languages) => {
    // The first language turns the choice on; none means any language.
    if (!chosen().length && languages.length) profile.language_mode = 'prefer';
    profile.languages = languages;
    if (!languages.length) Object.assign(profile, { language_mode: 'any', allow_unknown_language: true });
    paint();
    update();
  };
  const paint = () => {
    const languages = chosen();
    list.replaceChildren(...languages.map((value, index) => {
      const detail = AUDIO.find(([key]) => key === value)?.[2];
      const move = (step) => {
        const next = [...languages];
        [next[index], next[index + step]] = [next[index + step], next[index]];
        set(next);
      };
      return h('li', {},
        h('span', { class: 'audio-rank' }, String(index + 1)),
        h('span', { class: 'audio-name' }, h('strong', {}, audioName(value)), detail ? h('small', {}, detail) : null),
        h('span', { class: 'ladder-moves' },
          h('button', { type: 'button', class: 'icon-button', 'aria-label': `Move ${audioName(value)} up`, disabled: index === 0, onclick: () => move(-1) }, '↑'),
          h('button', { type: 'button', class: 'icon-button', 'aria-label': `Move ${audioName(value)} down`, disabled: index === languages.length - 1, onclick: () => move(1) }, '↓'),
          h('button', { type: 'button', class: 'icon-button', 'aria-label': `Remove ${audioName(value)}`, onclick: () => set(languages.filter((v) => v !== value)) }, icon('close'))));
    }));
    list.hidden = !languages.length;
    const group = (key, label) => {
      const left = AUDIO.filter(([value]) => audioGroup(value) === key && !languages.includes(value))
        .map(([value, name, detail]) => h('option', { value }, detail ? `${name} · ${detail}` : name));
      return left.length ? h('optgroup', { label }, left) : null;
    };
    add.replaceChildren(h('option', { value: '' }, languages.length ? 'Also needed…' : 'Any language. Add one…'),
      ...[group('title', 'Each title'), group('french', 'French'), group('other', 'Other languages')].filter(Boolean));
    add.value = '';
  };
  add.addEventListener('change', (event) => { event.stopPropagation(); if (add.value) set([...chosen(), add.value]); });
  paint();

  const choice = (options, current, pick) => h('div', { class: 'segmented', role: 'group' }, options.map(([value, label]) =>
    h('button', { type: 'button', 'aria-pressed': String(value === current()), dataset: { value }, onclick: () => pick(value) }, label)));
  const mode = () => (languageMode(profile) === 'require' ? 'require' : 'prefer');
  const others = choice([['prefer', 'Accepted until a complete one'], ['require', 'Refused']], mode, (value) => {
    profile.language_mode = value;
    if (value === 'prefer') profile.allow_unknown_language = true;
    update();
  });
  const unknown = check(id('lang-unknown'), profile.allow_unknown_language !== false, (on) => { profile.allow_unknown_language = on; update(); });
  const multi = check(id('multi'), hasExtra(profile, 'prefer', MULTI), (on) => { setExtra(profile, 'prefer', MULTI, on); update(); });
  const rows = [
    row(id('lang'), 'Languages', "A release needs every one: VFF and VOSTFR take a release with both, such as MULTi.VFF. Original language is each title's own audio: an English film then needs VO, MULTi or VOSTFR, or no language tag; a French film, French audio. They decide before the quality: a 1080p with both beats a 4K with VFF alone. None: any language.", h('div', { class: 'audio-choice' }, list, add)),
    row(id('lang-others'), 'Missing some', 'Accepted: the release with most of them comes first, the first ones counting most, and an upgrade replaces it once a release has them all. Refused: only a release with every one is downloaded.', others),
    row(id('lang-unknown'), 'MULTI and untagged', 'A release marked only MULTI, or without a language, does not say what it holds. On: they are accepted anyway.', unknown, true),
    row(id('multi'), 'MULTI first', 'Releases that also hold the original audio, such as MULTi.VFF, go before VFF alone.', multi, true),
  ];
  const [, othersRow, unknownRow] = rows;
  const refresh = () => {
    const none = !chosen().length;
    for (const button of others.children) button.setAttribute('aria-pressed', String(button.dataset.value === mode()));
    unknown.checked = profile.allow_unknown_language !== false;
    othersRow.hidden = none;
    unknownRow.hidden = none || mode() !== 'require';
  };
  hooks.push(refresh);
  refresh();
  return rows;
}

function sizeInput(id, profile, key, update) {
  const input = h('input', {
    id, type: 'text', inputMode: 'decimal', placeholder: 'No limit',
    value: profile[key] ? String(Math.round((profile[key] / GB) * 100) / 100) : '',
  });
  input.addEventListener('input', () => {
    const typed = input.value.trim().replace(',', '.');
    const mb = typed ? Math.round(Number(typed) * GB) : null;
    input.setCustomValidity(typed && (!Number.isFinite(mb) || mb < 1 || mb > 1_000_000) ? 'A size from 0.001 to 1000 GB' : '');
    if (!input.validationMessage) { profile[key] = mb; update(); }
  });
  return h('span', { class: 'with-unit' }, input, h('span', { class: 'unit', 'aria-hidden': 'true' }, 'GB/h'));
}

function extraChoices(profile, update) {
  return h('div', { class: 'choice-chips' }, EXTRAS.map(([key, label, kind, rules]) => h('label', { class: 'chip', dataset: { extra: key } },
    h('input', { type: 'checkbox', checked: hasExtra(profile, kind, rules), onchange: (event) => { setExtra(profile, kind, rules, event.target.checked); update(); } }),
    label)));
}

function setWords(profile, kind, list) {
  if (kind === 'avoid') {
    profile.blocked_terms = [...(profile.blocked_terms ?? []).filter((term) => EXTRA_TERMS.has(term)), ...list];
    return;
  }
  const extras = (profile.score_rules ?? []).filter((rule) => rule.terms?.length === 1 && EXTRA_TERMS.has(rule.terms[0]));
  profile.score_rules = [...extras, ...list.map((term) => ({ terms: [term], score: PREFER_SCORE }))];
}

/** Tries the profile as it is being edited on a real search, saving nothing. */
function tryOut(name, profile, hooks) {
  const title = h('input', { type: 'text', spellcheck: false, placeholder: 'A film, or a series with its episode: Severance S01E01', 'aria-label': 'Title to try' });
  // With "Original language" among the languages: TMDB's, found from the title, or one picked here.
  const original = h('select', { 'aria-label': 'Original language of the title to try' },
    h('option', { value: '' }, 'Original language from TMDB'),
    ...LANGUAGES.map(([value, label]) => h('option', { value }, `Original language: ${label}`)));
  const showOriginal = () => { original.hidden = !(profile.languages ?? []).includes('original'); };
  hooks.push(showOriginal);
  showOriginal();
  const run = h('button', { type: 'button', class: 'secondary' }, icon('search'), 'Try');
  const out = h('ol', { class: 'release-list try-out' });
  run.addEventListener('click', () => act(run, async () => {
    const typed = title.value.trim();
    if (!typed) throw new Error('Type a title to try the profile on');
    const episode = /^(.*?)[\s.]+s(\d{1,2})e(\d{1,3})$/i.exec(typed);
    const request = episode
      ? { kind: 'episode', title: episode[1].trim(), season: Number(episode[2]), episode: Number(episode[3]) }
      : { kind: 'movie', title: typed };
    const chosen = !original.hidden && original.value ? original.value : null;
    out.replaceChildren(h('li', { class: 'empty' }, 'Searching the indexers…'));
    const report = await link.call('POST', '/api/search', { ...request, ...(chosen ? { original_language: chosen } : {}), profile: clean(profile) });
    out.replaceChildren(...tryRows(report, Boolean(chosen)));
  }));
  return h('div', { class: 'setting-row try-row' },
    h('div', { class: 'setting-label' }, h('span', {}, 'Try it'), h('p', { class: 'hint' }, 'Searches your indexers with this profile as it is now, before you save it. Nothing is downloaded.')),
    h('div', {}, h('span', { class: 'with-unit try-input' }, title, original, run), out));
}

/** What a search knew of the title's original language, when the profile asks for it. */
export function originalLanguageText(report, chosen = false) {
  const original = report?.original_language;
  if (!original || typeof original !== 'object') return null;
  if (typeof original.name === 'string' && original.name) return `Original language: ${original.name}, ${chosen ? 'as chosen' : 'from TMDB'}`;
  return 'Original language unknown: TMDB did not give it, so only VO, MULTi and VOSTFR count as original audio';
}

/** Why a release has, or lacks, the title's original audio, when the profile
    asks for it; nothing when its reasons already say it. */
export function originalAudioLine(c) {
  const original = c.assessment?.original;
  const text = original?.explanation;
  if (typeof text !== 'string' || !text || (c.assessment.reasons ?? []).includes(text)) return null;
  return h('div', { class: 'release-original', dataset: original.audio ? {} : { tone: 'warn' } }, text);
}

/** A profile as the server reads it: the label and unset values left out when empty. */
function clean(profile) {
  const copy = structuredClone(profile);
  for (const [key, value] of Object.entries(copy)) if (value === undefined) delete copy[key];
  return copy;
}

/** The pick, the closest contenders and why they lost, then the rejections. */
function tryRows(report, chosen) {
  const accepted = Array.isArray(report.accepted) ? report.accepted : [];
  const rejected = Array.isArray(report.rejected) ? report.rejected : [];
  const origin = originalLanguageText(report, chosen);
  const head = origin ? [h('li', { class: 'empty' }, origin)] : [];
  if (!accepted.length && !rejected.length) return [...head, h('li', { class: 'empty' }, 'The indexers found nothing for this title.')];
  const pick = report.selected_candidate_id;
  const rows = [...head, ...accepted.slice(0, 5).map((c) => h('li', { class: `release${c.id === pick ? ' chosen' : ''}` },
    h('div', { class: 'release-main' },
      c.id === pick ? h('span', { class: 'badge', dataset: { tone: 'accent' } }, "Mynou's pick") : null,
      h('div', { class: 'release-title' }, c.title ?? '(untitled)'),
      originalAudioLine(c),
      h('div', { class: 'release-why' }, c.id === pick ? sizeFacts(c) : [lostText(c.lost_on), sizeFacts(c)].filter(Boolean).join(' · ')))))];
  if (!accepted.length) rows.push(h('li', { class: 'empty' }, 'No release meets this profile.'));
  if (rejected.length) {
    const reasons = new Map();
    for (const c of rejected) for (const reason of c.assessment?.reasons ?? []) reasons.set(reason, (reasons.get(reason) ?? 0) + 1);
    rows.push(h('li', { class: 'empty' }, `${rejected.length} rejected: ${[...reasons].sort((a, b) => b[1] - a[1]).slice(0, 3).map(([reason, n]) => `${reason} (${n})`).join(' · ')}`));
  }
  return rows;
}

// Why an accepted release came after Mynou's pick: the first criterion where it was worse.
const LOST_ON = {
  resolution: 'Lower quality', source: 'Same quality, lower source', language: 'Fewer of your languages',
  custom_score: 'Fewer preferred words', revision: 'Not a proper or repack', codec: 'Less preferred codec',
  freeleech: 'Not freeleech', priority: 'Lower indexer priority', seeders: 'Fewer seeders',
};
export const lostText = (key) => (key ? LOST_ON[key] ?? `Behind on ${key}` : null);

/** Size and size per hour of a candidate, when known. */
export function sizeFacts(c) {
  const size = Number(c.size_bytes);
  const perHour = Number(c.mb_per_hour);
  return [size > 0 ? `${Math.round(size / 1e8) / 10} GB` : null, perHour > 0 ? `${Math.round(perHour / 100) / 10} GB/h` : null].filter(Boolean).join(' · ');
}

/** Folds the Quality page into the settings document, when it changed. */
export function readQuality(document) {
  if (!draft || JSON.stringify(draft) === saved) return;
  for (const [name, profile] of Object.entries(draft.profiles)) {
    if (!profile.label?.trim()) delete profile.label;
    if (profile.min_mb_per_hour && profile.max_mb_per_hour && profile.min_mb_per_hour > profile.max_mb_per_hour) {
      throw new Error(`${profileLabel(name, profile)}: the smallest size is above the largest`);
    }
    const terms = [...(profile.blocked_terms ?? []), ...(profile.required_terms ?? []), ...(profile.score_rules ?? []).flatMap((rule) => rule.terms)];
    if (terms.some((term) => new TextEncoder().encode(term).length > 128)) throw new Error(`${profileLabel(name, profile)}: each word holds at most 128 bytes`);
  }
  document.selection = structuredClone(draft);
}

// A film or a series can use another profile than the default one.
let bindings = null;

async function binding(kind, tmdbId) {
  if (!bindings) {
    // Kept only once every page arrived: a failed read leaves no partial list.
    // 500 pages of 200 hold the 100,000 bindings the server allows.
    const loaded = new Map();
    let after = null;
    for (let page = 0; page < 500; page++) {
      const reply = await link.call('GET', `/api/title-profiles?limit=200${after ? `&after=${encodeURIComponent(after)}` : ''}`);
      for (const b of reply.bindings ?? []) loaded.set(`${b.kind}:${b.tmdb_id}`, b.profile);
      after = reply.next;
      if (!after) break;
    }
    if (after) throw new Error('Too many titles have their own profile to show this one');
    bindings ??= loaded;
  }
  return bindings.get(`${kind}:${tmdbId}`) ?? null;
}

async function bind(kind, tmdbId, profile) {
  const body = profile ? { action: 'set', kind, tmdb_id: tmdbId, profile } : { action: 'clear', kind, tmdb_id: tmdbId };
  const plan = await link.call('POST', '/api/title-profiles/control', body);
  await link.call('POST', '/api/title-profiles/control', { ...body, apply: true, plan_id: plan.plan_id });
  if (profile) bindings?.set(`${kind}:${tmdbId}`, profile); else bindings?.delete(`${kind}:${tmdbId}`);
}

/** Gives one title its own profile, or the default one again with null. */
export const bindTitle = (kind, tmdbId, profile) => bind(kind, Number(tmdbId), profile);

/** Notes a profile chosen when a title was added, so its sheet shows it at once. */
export function remember(kind, tmdbId, profile) {
  if (profile) bindings?.set(`${kind}:${tmdbId}`, profile);
}

/** The saved profiles, read once per page load. */
let selectionRead = null;
export const savedProfiles = () => (selectionRead ??= link.call('GET', '/api/profiles').catch((error) => { selectionRead = null; throw error; }));
export const forgetProfiles = () => { selectionRead = null; };

/** A profile picker for adding a film (`movie`) or series (`series`), set on the default. */
export function profilePicker(kind) {
  const select = h('select', { class: 'profile-picker', 'aria-label': 'Quality profile', disabled: true }, h('option', {}, 'Loading…'));
  savedProfiles().then((selection) => {
    const fallback = selection[kind === 'movie' ? 'movie_profile' : 'episode_profile'];
    select.replaceChildren(...Object.entries(selection.profiles ?? {}).map(([name, profile]) =>
      h('option', { value: name, selected: name === fallback, title: profileSummary(profile) }, profileLabel(name, profile))));
    select.dataset.fallback = fallback ?? '';
    select.disabled = false;
  }).catch(() => select.replaceChildren(h('option', { value: '' }, 'Default profile')));
  return select;
}

/** The chosen profile of a picker, or null when it is the default one. */
export const pickedProfile = (select) => (select && !select.disabled && select.value && select.value !== select.dataset.fallback ? select.value : null);

/** The profile menu of a film (`movie`) or series (`series`) sheet. */
export function titleQuality(kind, tmdbId) {
  const id = Number(tmdbId);
  if (!Number.isSafeInteger(id) || id < 1) return null;
  const select = h('select', { 'aria-label': 'Quality profile for this title', disabled: true }, h('option', {}, 'Loading…'));
  const hint = h('p', { class: 'hint' }, 'Applies to the next searches and upgrade checks; what is already downloaded stays.');
  const about = h('p', { class: 'hint' });
  let profiles = {};
  let fallback = null;
  const describe = () => { about.textContent = profileSummary(profiles[select.value || fallback] ?? {}); };
  Promise.all([savedProfiles(), binding(kind, id)]).then(([selection, bound]) => {
    profiles = selection.profiles ?? {};
    fallback = selection[kind === 'movie' ? 'movie_profile' : 'episode_profile'];
    const missing = bound && !profiles[bound] ? [h('option', { value: bound, selected: true }, `${bound} (no longer saved)`)] : [];
    select.replaceChildren(h('option', { value: '', selected: !bound }, `Default: ${profileLabel(fallback, profiles[fallback])}`), ...missing,
      ...Object.entries(profiles).map(([name, profile]) => h('option', { value: name, selected: bound === name }, profileLabel(name, profile))));
    select.disabled = false;
    select.dataset.shown = select.value;
    describe();
  }).catch((error) => { hint.textContent = error.message; hint.classList.add('bad'); });
  select.addEventListener('change', () => {
    select.disabled = true;
    describe();
    bind(kind, id, select.value || null)
      .then(() => { select.dataset.shown = select.value; toast('Profile saved for this title'); })
      .catch((error) => { select.value = select.dataset.shown ?? ''; describe(); toast(error.message, 'bad'); })
      .finally(() => { select.disabled = false; });
  });
  return h('div', { class: 'setting-list' },
    h('div', { class: 'setting-row' }, h('div', { class: 'setting-label' }, h('span', {}, 'Quality profile'), about, hint), select));
}
