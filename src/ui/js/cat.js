import { $ } from './dom.js';

// Mynou's cat: a 16 × 16 pixel sprite drawn from the rows below, and the quiet
// life it leads wherever it sits. In a row, '#' is fur, 'g' the glow of an eye
// and '.' nothing; a part is [first row, ...rows].

const ART = {
  body: [3,
    '..#######',
    '.#########',
    '.#########',
    '.#########',
    '..#######',
    '...#####',
    '..#######',
    '..#######',
    '.#########',
    '.#########',
    '.#########',
    '.#########',
    '..###.###'],
  ears: {
    up: [1, '..#.....#', '..##...##'],
    // The left ear flicks out for a moment.
    twitch: [1, '.#......#', '..##...##'],
    // Laid back: shorter and wider.
    back: [2, '.##.....##'],
  },
  eyes: {
    open: [5, '...g...g', '...g...g'],
    // Half-closed, on the way to a blink and back.
    half: [6, '...g...g'],
    shut: [5, ''],
  },
  tail: {
    rest: [8, '............#', '............#', '.............#', '.............#', '.............#', '............#', '..........##', '.........##'],
    flick: [8, '...........#', '............#', '.............#', '.............#', '.............#', '............#', '..........##', '.........##'],
    up: [5, '...........#', '............#', '............#', '............#', '............#', '.............#', '.............#', '.............#', '............#', '..........##', '.........##'],
    // Lying along the floor, the tip turned up.
    low: [13, '..............#', '..............#', '.........#####'],
  },
};
const INKS = { '#': 'fur', g: 'eye' };

/** Path data per ink for parts laid in order, each [[first row, ...rows], dx, dy]. */
function paths(parts) {
  const grid = new Map();
  for (const [[top, ...rows], dx = 0, dy = 0] of parts) {
    rows.forEach((row, i) => {
      for (let x = 0; x < row.length; x++) if (row[x] !== '.') grid.set(`${x + dx},${top + i + dy}`, row[x]);
    });
  }
  const out = Object.fromEntries(Object.values(INKS).map((ink) => [ink, '']));
  for (let y = 0; y < 16; y++) {
    for (let x = 0; x < 16; x++) {
      const ink = grid.get(`${x},${y}`);
      if (!ink || grid.get(`${x - 1},${y}`) === ink) continue;
      let run = 1;
      while (grid.get(`${x + run},${y}`) === ink) run++;
      out[INKS[ink]] += `M${x} ${y}h${run}v1h-${run}z`;
    }
  }
  return out;
}

const REST = { ears: 'up', eyes: 'open', tail: 'rest', look: null };
/** The path data of a pose: `REST` with some parts changed. */
export function draw(pose = {}) {
  const p = { ...REST, ...pose };
  return paths([[ART.body], [ART.tail[p.tail]], [ART.ears[p.ears]], [ART.eyes[p.eyes], ...(p.look ?? [0, 0])]]);
}

// ---------------------------------------------------------------- life --

const reduce = matchMedia('(prefers-reduced-motion: reduce)');
const cats = new Set();
// A mood holds a pose until the next one; a moment (a blink, a flick) passes.
const MOODS = {
  awake: {},
  asleep: { eyes: 'shut', tail: 'low' },
  shy: { eyes: 'shut' },
  search: {},
  offline: { ears: 'back', tail: 'low', look: [0, 1] },
};
// Awake until the link says otherwise: a sign-in waits with the cat looking on.
const state = { link: 'awake', asleep: false, shy: false };
const SLEEP_AFTER = 75_000;
let lastInput = performance.now();
let pointer = null;
const rand = (low, high) => low + Math.random() * (high - low);

function mood() {
  if (state.shy) return 'shy';
  if (state.link !== 'awake') return state.link;
  return state.asleep ? 'asleep' : 'awake';
}
const shown = (cat) => !document.hidden && cat.svg.isConnected && cat.svg.getClientRects().length > 0;

function paint(cat) {
  const now = mood();
  const pose = { ...MOODS[now], ...cat.moment };
  if (!pose.look) pose.look = cat.look;
  const key = JSON.stringify(pose);
  if (key !== cat.drawn) {
    cat.drawn = key;
    const d = draw(pose);
    for (const [ink, path] of Object.entries(cat.paths)) path.setAttribute('d', d[ink]);
  }
  if (cat.svg.dataset.mood !== now) cat.svg.dataset.mood = now;
}

/** Plays poses one after the other, [pose, milliseconds] each, then rests. */
function play(cat, steps) {
  clearTimeout(cat.timer);
  const next = (i) => {
    cat.moment = steps[i]?.[0] ?? null;
    paint(cat);
    if (i < steps.length) cat.timer = setTimeout(() => next(i + 1), steps[i][1]);
  };
  next(0);
}

/** A small spring up and a soft landing, `height` in pixels of the sprite. */
function hop(cat, height = 2) {
  if (reduce.matches || !cat.sprite.animate) return;
  cat.hop?.cancel();
  cat.hop = cat.sprite.animate([
    { transform: 'none' },
    { transform: 'scale(1.04, .95)', offset: 0.15 },
    { transform: `translateY(-${height}px) scale(.98, 1.03)`, offset: 0.45 },
    { transform: 'scale(1.03, .97)', offset: 0.8 },
    { transform: 'none' },
  ], { duration: 520, easing: 'cubic-bezier(.3,.7,.4,1)' });
}

const BLINK = [[{ eyes: 'half' }, 45], [{ eyes: 'shut' }, 90], [{ eyes: 'half' }, 55]];
const DOUBLE_BLINK = [...BLINK, [{}, 140], ...BLINK];
// A cat's slow blink: content, and saying so.
const SLOW_BLINK = [[{ eyes: 'half', tail: 'up' }, 160], [{ eyes: 'shut', tail: 'up' }, 600], [{ eyes: 'half', tail: 'up' }, 200], [{ tail: 'up' }, 600]];
const FLICK = [[{ tail: 'flick' }, 170], [{}, 160], [{ tail: 'flick' }, 170]];
const TWITCH = [[{ ears: 'twitch' }, 120], [{}, 100], [{ ears: 'twitch' }, 110]];

/** One small thing now and then, so the cat never looks painted. */
function fidget(cat) {
  if (reduce.matches || !shown(cat) || cat.moment) return;
  // A pointer gone still for a while: the eyes come back to the middle.
  if (cat.look && (!pointer || performance.now() - pointer.at > 6000)) { cat.look = null; paint(cat); }
  const now = mood();
  if (now === 'asleep') { if (Math.random() < 0.25) play(cat, TWITCH); return; }
  if (now === 'search') {
    cat.side = cat.side === 1 ? -1 : 1;
    play(cat, [[{ look: [cat.side, 0] }, 700]]);
    return;
  }
  if (now !== 'awake') return;
  const roll = Math.random();
  const watching = pointer && performance.now() - pointer.at < 4000;
  if (roll < 0.42) play(cat, Math.random() < 0.2 ? DOUBLE_BLINK : BLINK);
  else if (roll < 0.64) play(cat, FLICK);
  else if (roll < 0.8) play(cat, TWITCH);
  else if (roll < 0.94 && !watching) play(cat, [[{ look: [Math.random() < 0.5 ? -1 : 1, Math.random() < 0.3 ? -1 : 0] }, rand(900, 1700)], ...BLINK]);
  else play(cat, SLOW_BLINK);
}
function live(cat) {
  const now = mood();
  const delay = now === 'search' ? 760 : now === 'asleep' ? rand(3000, 6000) : rand(1800, 4600);
  cat.idle = setTimeout(() => { fidget(cat); live(cat); }, delay);
}

/** Where the pointer or the caret is, the eyes go: one pixel toward it. */
function look() {
  if (reduce.matches) return;
  for (const cat of cats) {
    if (!shown(cat)) continue;
    let next = null;
    if (pointer && performance.now() - pointer.at < 6000) {
      const r = cat.svg.getBoundingClientRect();
      const dx = pointer.x - (r.left + r.width * 0.35), dy = pointer.y - (r.top + r.height * 0.35);
      const near = Math.max(r.width * 0.6, 16);
      next = Math.hypot(dx, dy) < near ? null : [Math.abs(dx) > near ? Math.sign(dx) : 0, Math.abs(dy) > near * 1.6 ? Math.sign(dy) : 0];
    }
    if (String(next) !== String(cat.look)) { cat.look = next; paint(cat); }
  }
}

function wake() {
  state.asleep = false;
  for (const cat of cats) {
    if (!shown(cat) || reduce.matches) { play(cat, []); continue; }
    play(cat, [[{ eyes: 'shut', ears: 'twitch' }, 260], [{}, 160], ...BLINK]);
    hop(cat, 1);
  }
}

let looking = 0;
function input(event) {
  lastInput = performance.now();
  if (event.type === 'pointermove' || event.type === 'pointerdown') {
    pointer = { x: event.clientX, y: event.clientY, at: lastInput };
  } else if (event.type === 'input' && event.target instanceof Element) {
    // Typing draws the eyes to the field.
    const r = event.target.getBoundingClientRect();
    pointer = { x: r.left + Math.min(r.width, 240), y: r.top + r.height / 2, at: lastInput };
  }
  if (state.asleep) wake();
  if (!looking) looking = requestAnimationFrame(() => { looking = 0; look(); });
}

/** Gives every `svg.logo` under `root` its sprite and its life. */
export function mountCats(root = document) {
  const template = $('#cat-template').content.firstElementChild;
  for (const svg of root.querySelectorAll('svg.logo:not([data-mood])')) {
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.replaceChildren(...template.cloneNode(true).childNodes);
    const sprite = $('.cat-sprite', svg);
    const cat = {
      svg, sprite, paths: Object.fromEntries(Object.values(INKS).map((ink) => [ink, $(`.cat-${ink}`, sprite)])),
      moment: null, look: null, drawn: '', timer: 0, idle: 0,
    };
    cats.add(cat);
    paint(cat);
    live(cat);
    // A pointer that comes to rest on it gets a slow blink.
    (svg.closest('a, button') ?? svg).addEventListener('pointerenter', (event) => {
      if (event.pointerType === 'mouse' && mood() === 'awake' && !reduce.matches) play(cat, SLOW_BLINK);
    });
  }
}

/** A word from the rest of the page: 'happy' for a success, 'wince' for an error. */
export function react(feeling) {
  if (reduce.matches || mood() === 'asleep' || mood() === 'offline') return;
  for (const cat of cats) {
    if (!shown(cat)) continue;
    if (feeling === 'happy') {
      play(cat, [[{ tail: 'up' }, 500], ...SLOW_BLINK]);
      hop(cat);
    } else if (feeling === 'wince') {
      play(cat, [[{ ears: 'back', eyes: 'shut' }, 160], [{ ears: 'back' }, 1300]]);
      if (!reduce.matches) cat.sprite.animate?.([{ transform: 'none' }, { transform: 'translateX(-1px)' }, { transform: 'translateX(1px)' }, { transform: 'none' }], { duration: 240, easing: 'steps(1)' });
    }
  }
}

/** The link to the server: 'open', 'connecting' or 'closed'. */
export function linkMood(detail) {
  const was = state.link;
  state.link = { open: 'awake', connecting: 'search', closed: 'offline' }[detail] ?? 'search';
  if (state.link === 'awake') lastInput = performance.now();
  for (const cat of cats) {
    if (state.link === 'awake' && was !== 'awake' && shown(cat) && !reduce.matches) {
      play(cat, [[{ tail: 'up' }, 600], ...BLINK]);
      hop(cat);
    } else play(cat, []);
  }
}

/** While a password is typed, every cat looks away. */
export function shy(on) {
  if (state.shy === on) return;
  state.shy = on;
  for (const cat of cats) play(cat, []);
}

export function setupCats() {
  mountCats();
  for (const type of ['pointermove', 'pointerdown', 'keydown', 'wheel', 'input']) addEventListener(type, input, { passive: true, capture: true });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) lastInput = performance.now(); });
  setInterval(() => {
    if (state.asleep || state.shy || state.link !== 'awake' || performance.now() - lastInput < SLEEP_AFTER) return;
    state.asleep = true;
    for (const cat of cats) play(cat, []);
  }, 5000);
  reduce.addEventListener('change', () => { for (const cat of cats) { cat.look = null; play(cat, []); } });
}
