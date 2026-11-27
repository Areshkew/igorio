'use strict';

// The "Pay what you want" page. The button stays hidden while this is empty.
const SUPPORT_URL = 'https://buy.polar.sh/polar_cl_fD77eebHRUazKLnDC7GmUVxaOvp0thmQb2wg04NjlfL';

const REPO = 'Areshkew/igorio';

// ?t=<seconds> starts the demo there; add &still=1 to show only that frame.
// ?record=1 shows only the demo and its caption, for recording (see tools/record.py).
// ?og=1 lays the page out as the 1200 x 630 Open Graph picture.
// ?stars=mock (37 stars) or ?stars=mock-big (2,400) uses sample data instead of the GitHub API.
const params = new URLSearchParams(location.search);
const RECORD = params.has('record');
const OG = params.has('og');
const STILL = params.has('still') || RECORD;
const MOCK = /^mock(-big)?$/.test(params.get('stars')) ? params.get('stars') : '';
const DAY = 864e5;
if (RECORD) document.body.classList.add('record');
if (OG) document.body.classList.add('og');

// ---------------------------------------------------------------------------
// Header

(function header() {
  const support = document.getElementById('support');
  if (SUPPORT_URL) {
    support.href = SUPPORT_URL;
    support.hidden = false;
  }
  if (STILL || OG) return;
  gh('')
    .then(({ data }) => {
      const n = data && data.stargazers_count;
      if (typeof n !== 'number') return;
      const text = n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1).replace(/\.0$/, '') + 'k' : String(n);
      document.getElementById('star-count').textContent = text;
      document.getElementById('stars').hidden = false;
      document.getElementById('github').setAttribute('aria-label', `GitHub, ${n} stars`);
      starPopover(n);
    })
    .catch(() => {});
})();

// ---------------------------------------------------------------------------
// Star history: a small chart under the GitHub button, on hover or keyboard focus.

// GET from the repo's API (or the mock). Resolves to { data, link }.
function gh(path, accept = 'application/vnd.github+json') {
  if (MOCK) return mockGh(path);
  return fetch('https://api.github.com/repos/' + REPO + path, { headers: { Accept: accept } }).then((r) =>
    r.ok ? r.json().then((data) => ({ data, link: r.headers.get('Link') || '' })) : Promise.reject(new Error(String(r.status))),
  );
}

// Sample stars: a launch bump, then steady, served in pages of 100 like the API.
function mockGh(path) {
  const big = MOCK === 'mock-big';
  const n = big ? 2400 : 37, span = (big ? 425 : 92) * DAY, now = Date.now();
  const at = (i) => {
    const u = (i + 0.5) / n;
    const f = (u < 0.3 ? u / 3 : 0.1 + ((u - 0.3) * 9) / 7) + 0.01 * Math.sin(u * 20);
    return new Date(now - DAY - span * (1 - f)).toISOString();
  };
  const m = /[?&]page=(\d+)/.exec(path);
  const last = Math.ceil(n / 100), p = m && +m[1];
  const res = !m
    ? { data: { stargazers_count: n }, link: '' }
    : { data: Array.from({ length: Math.max(0, Math.min(100, n - (p - 1) * 100)) }, (_, j) => ({ starred_at: at((p - 1) * 100 + j) })),
        link: last > 1 ? `<https://api.github.com/x?per_page=100&page=${last}>; rel="last"` : '' };
  return new Promise((done) => setTimeout(done, 200, res));
}

// Points [time, stars so far], oldest first, ending with [now, total]. At most 6 requests:
// up to 600 stars every page (one point per star); above that, 6 pages spread evenly from
// the first to the last (one point per page: its first star). Cached for an hour.
async function loadStarHistory(total) {
  const key = 'igorio-stars-' + (MOCK || REPO);
  try {
    const c = JSON.parse(sessionStorage.getItem(key));
    if (c && c.total === total && Date.now() - c.at < 3600e3) return c.points;
  } catch (e) {}
  const page = (p) => gh(`/stargazers?per_page=100&page=${p}`, 'application/vnd.github.star+json');
  const time = (s) => Date.parse(s.starred_at);
  const first = await page(1);
  const lastLink = /<([^>]*)>;\s*rel="last"/.exec(first.link);
  const last = lastLink ? +(/[?&]page=(\d+)/.exec(lastLink[1]) || [])[1] || 1 : 1;
  let points;
  if (last <= 6) {
    const rest = await Promise.all(Array.from({ length: last - 1 }, (_, i) => page(i + 2)));
    points = [first, ...rest].flatMap((r) => r.data).map((s, i) => [time(s), i + 1]);
  } else {
    const nums = [1, 2, 3, 4, 5].map((k) => 1 + Math.round((k * (last - 1)) / 5));
    const rest = await Promise.all(nums.map(page));
    points = [first, ...rest].map((r, k) => [time(r.data[0]), (k ? nums[k - 1] - 1 : 0) * 100 + 1]);
  }
  points = points.filter((p) => p[0] > 0);
  if (!points.length) throw new Error('no stars');
  points.push([Date.now(), Math.max(total, points[points.length - 1][1])]);
  try {
    sessionStorage.setItem(key, JSON.stringify({ at: Date.now(), total, points }));
  } catch (e) {}
  return points;
}

// Stars at time t, read straight between the points.
function starsAt(points, t) {
  if (t < points[0][0]) return 0;
  for (let i = 1; i < points.length; i++) {
    const [t1, c1] = points[i];
    if (t <= t1) {
      const [t0, c0] = points[i - 1];
      return t1 > t0 ? c0 + ((c1 - c0) * (t - t0)) / (t1 - t0) : c1;
    }
  }
  return points[points.length - 1][1];
}

// A monotone cubic through P ([x, y], x rising): no overshoot, so the line never dips.
function monotone(P) {
  const n = P.length, s = [], m = [];
  for (let i = 0; i < n - 1; i++) s.push((P[i + 1][1] - P[i][1]) / (P[i + 1][0] - P[i][0]));
  m[0] = s[0];
  m[n - 1] = s[n - 2];
  for (let i = 1; i < n - 1; i++) {
    const h0 = P[i][0] - P[i - 1][0], h1 = P[i + 1][0] - P[i][0];
    const p = (s[i - 1] * h1 + s[i] * h0) / (h0 + h1);
    m[i] = (Math.sign(s[i - 1]) + Math.sign(s[i])) * Math.min(Math.abs(s[i - 1]), Math.abs(s[i]), 0.5 * Math.abs(p)) || 0;
  }
  const f = (v) => v.toFixed(2);
  let d = `M${f(P[0][0])} ${f(P[0][1])}`;
  for (let i = 0; i < n - 1; i++) {
    const h = (P[i + 1][0] - P[i][0]) / 3;
    d += `C${f(P[i][0] + h)} ${f(P[i][1] + m[i] * h)} ${f(P[i + 1][0] - h)} ${f(P[i + 1][1] - m[i + 1] * h)} ${f(P[i + 1][0])} ${f(P[i + 1][1])}`;
  }
  // y at x, on the same curve.
  const yAt = (x) => {
    let i = 0;
    while (i < n - 2 && x > P[i + 1][0]) i++;
    const h = P[i + 1][0] - P[i][0], u = clamp((x - P[i][0]) / h, 0, 1);
    const a = u * u * (3 - 2 * u);
    return P[i][1] * (1 - a) + P[i + 1][1] * a + h * (m[i] * u * (1 - u) * (1 - u) - m[i + 1] * u * u * (1 - u));
  };
  return { d, yAt };
}

function renderStarPopover(pop, total, points) {
  const plural = (n) => n.toLocaleString('en-US') + (n === 1 ? ' star' : ' stars');
  const now = Date.now();
  const recent = points.length > 1 ? total - Math.floor(starsAt(points, now - 30 * DAY)) : 0;
  const change = recent > 0 ? `+${recent.toLocaleString('en-US')} in the last 30 days` : '';
  pop.setAttribute('aria-label', `${plural(total)} on GitHub` + (change ? ', ' + change.slice(1) : ''));
  if (points.length < 2) {
    pop.innerHTML = `<div class="sp-total">${plural(total)}</div><div class="sp-sub">Be the first to star Igorio</div>`;
    return;
  }

  // Chart box, in px. Up to 60 points are drawn as they are; more are read at 60 even steps.
  const W = 260, H = 96, PX = 3, TOP = 6, BOT = 1;
  const t0 = points[0][0], t1 = Math.max(points[points.length - 1][0], t0 + DAY);
  const x = (t) => PX + ((t - t0) / (t1 - t0)) * (W - 2 * PX);
  const y = (c) => H - BOT - (c / Math.max(total, 1)) * (H - TOP - BOT);
  const series = points.length <= 60 ? points : Array.from({ length: 60 }, (_, i) => t0 + ((t1 - t0) * i) / 59).map((t) => [t, starsAt(points, t)]);
  const P = [];
  for (const [t, c] of series) {
    const p = [x(t), y(c)];
    if (P.length && p[0] - P[P.length - 1][0] < 0.5) P[P.length - 1] = [P[P.length - 1][0], p[1]];
    else P.push(p);
  }
  if (P.length < 2) P.unshift([PX, y(0)]);
  const curve = monotone(P);
  const end = P[P.length - 1];
  const month = new Date(t0).toLocaleDateString('en-US', { month: 'short', year: 'numeric' });

  pop.innerHTML = `<div class="sp-total">${plural(total)}</div>${change ? `<div class="sp-sub">${change}</div>` : ''}
    <svg class="sp-chart" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" aria-hidden="true">
      <defs><linearGradient id="sp-fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0" class="sp-stop-a"/><stop offset="1" class="sp-stop-b"/></linearGradient></defs>
      <path class="sp-area" d="${curve.d}L${end[0].toFixed(2)} ${H}L${P[0][0].toFixed(2)} ${H}Z" fill="url(#sp-fill)"/>
      <line class="sp-base" x1="0" x2="${W}" y1="${H - 0.5}" y2="${H - 0.5}"/>
      <path class="sp-line" d="${curve.d}"/>
      <line class="sp-rule" y1="0" y2="${H}"/>
      <circle class="sp-dot" r="3.5"/>
    </svg>
    <div class="sp-axis"><span>${month}</span><span>now</span><span class="sp-read"></span></div>`;

  const svg = pop.querySelector('svg'), rule = svg.querySelector('.sp-rule'), dot = svg.querySelector('.sp-dot');
  const axis = pop.querySelector('.sp-axis'), read = pop.querySelector('.sp-read');
  svg.addEventListener('pointermove', (e) => {
    const r = svg.getBoundingClientRect();
    const px = clamp(((e.clientX - r.left) / r.width) * W, P[0][0], end[0]);
    const py = curve.yAt(px);
    const t = t0 + ((px - PX) / (W - 2 * PX)) * (t1 - t0);
    const c = clamp(Math.round(((H - BOT - py) / (H - TOP - BOT)) * total), 0, total);
    rule.setAttribute('x1', px.toFixed(2));
    rule.setAttribute('x2', px.toFixed(2));
    dot.setAttribute('cx', px.toFixed(2));
    dot.setAttribute('cy', py.toFixed(2));
    const date = new Date(t).toLocaleDateString('en-US', { month: 'short', day: 'numeric', year: 'numeric' });
    read.textContent = `${date} · ${plural(c)}`;
    read.style.left = clamp(px - read.offsetWidth / 2, 0, W - read.offsetWidth) + 'px';
    axis.classList.add('reading');
  });
  svg.addEventListener('pointerleave', () => axis.classList.remove('reading'));
}

function starPopover(total) {
  const btn = document.getElementById('github');
  const pop = document.getElementById('star-pop');
  if (!matchMedia('(hover: hover)').matches) return;
  let ready = null, hover = false, focus = false, timer = 0;

  // Loads once, on the first hover or focus. Any error: the popover never opens.
  const load = () =>
    (ready ||= (total < 2 ? Promise.resolve([]) : loadStarHistory(total)).then(
      (points) => {
        renderStarPopover(pop, total, points);
        btn.setAttribute('aria-describedby', 'star-pop');
        return true;
      },
      () => false,
    ));

  function place() {
    const nav = pop.offsetParent.getBoundingClientRect();
    const left = btn.offsetLeft + btn.offsetWidth / 2 - pop.offsetWidth / 2;
    pop.style.left = clamp(left, 16 - nav.left, innerWidth - 16 - nav.left - pop.offsetWidth) + 'px';
    pop.style.top = btn.offsetTop + btn.offsetHeight + 8 + 'px';
  }

  // Open after a short hover, close after a short grace, so the pointer can cross the gap.
  function sync(delay) {
    clearTimeout(timer);
    const on = hover || focus;
    if (on) load();
    timer = setTimeout(() => {
      if (!on) return pop.classList.remove('open');
      load().then((ok) => {
        if (!ok || !(hover || focus) || pop.classList.contains('open')) return;
        place();
        pop.classList.add('open');
      });
    }, delay);
  }
  const isOpen = () => pop.classList.contains('open');
  btn.addEventListener('pointerenter', (e) => {
    if (e.pointerType !== 'mouse') return;
    hover = true;
    sync(isOpen() ? 0 : 150);
  });
  pop.addEventListener('pointerenter', () => {
    hover = true;
    sync(0);
  });
  for (const node of [btn, pop]) {
    node.addEventListener('pointerleave', () => {
      hover = false;
      sync(120);
    });
  }
  btn.addEventListener('focus', () => {
    if (!btn.matches(':focus-visible')) return;
    focus = true;
    sync(150);
  });
  btn.addEventListener('blur', () => {
    focus = false;
    sync(0);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || !(hover || focus || isOpen())) return;
    hover = focus = false;
    clearTimeout(timer);
    pop.classList.remove('open');
  });
}

// ---------------------------------------------------------------------------
// The demo. Everything on the stage is a pure function of time: renderAt(t).

const T = 43;

const SCENES = [
  { title: 'Take a screenshot', keys: '[Win][Shift][S] or [PrtScn]', text: 'It turns into a small card that follows your cursor.', start: 0, end: 6.5, still: 4.6 },
  { title: 'Double-click to drop', keys: 'double-click', text: 'On a text box it pastes. On a folder or the desktop it saves a PNG.', start: 6.5, end: 11, still: 9.7 },
  { title: 'Shake to let go', keys: 'shake', text: 'Shake once to drop the top card. Keep shaking to drop them all.', start: 11, end: 16, still: 14.25 },
  { title: 'Notes', keys: 'hold [Alt]', text: 'Hold Alt anywhere except a text box, then pick New note. Notes take Markdown.', start: 16, end: 25, still: 22.8 },
  { title: 'Pin to screen', keys: 'hold [Alt]', text: 'Keep a screenshot on top while you work. Drag to move, scroll to resize.', start: 25, end: 33, still: 31.95 },
  { title: 'Record the screen', keys: '[Ctrl][Shift][6]', text: 'Records the screen under the cursor to MP4. Zoom with Alt + scroll, draw with Ctrl+Shift+7.', start: 33, end: 43, still: 37.9 },
];

// Keys shown on the screen while they are pressed: [from, to, keys].
const HUD = [
  [0.45, 1.6, '[Win][Shift][S]'],
  [8.0, 8.8, 'double-click'],
  [11.0, 11.7, '[PrtScn]'],
  [13.2, 14.5, 'shake'],
  [16.85, 18.15, 'hold [Alt]'],
  [25.25, 25.95, '[PrtScn]'],
  [27.85, 29.05, 'hold [Alt]'],
  [31.05, 31.9, '{wheel} scroll'],
  [33.85, 34.65, '[Ctrl][Shift][6]'],
  [35.0, 36.05, '[Alt] + {wheel} scroll'],
  [36.2, 36.95, '[Ctrl][Shift][7]'],
  [38.3, 39.0, '[Ctrl][Shift][6]'],
];

// --- Motion helpers ---------------------------------------------------------

const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const ramp = (t, a, b) => clamp((t - a) / (b - a), 0, 1);
const mix = (a, b, p) => a + (b - a) * p;
const inOut = (p) => (p < 0.5 ? 4 * p * p * p : 1 - Math.pow(-2 * p + 2, 3) / 2);
// In and out with a hair of overshoot at the end, so a stop has weight.
function settle(p) {
  const out = 1 + 2.15 * Math.pow(p - 1, 3) + 1.15 * Math.pow(p - 1, 2);
  return p < 0.5 ? inOut(p) : inOut(p) * (1 - (p - 0.5) * 2) + out * (p - 0.5) * 2;
}
// 0 outside [a, b], 1 inside, with short fades.
const span = (t, a, b, fi = 0.15, fo = 0.15) => ramp(t, a, a + fi) * (1 - ramp(t, b - fo, b));

// Stage geometry, in stage units (the screen is 960 x 600).
const PIC = { x: 80, y: 208, w: 300, h: 188 };
const SUN = { x: PIC.x + (237 * PIC.w) / 320, y: PIC.y + (60 * PIC.h) / 200 };
const THUMB = { x: 91, y: 465, w: 52, h: 32 };
const CARD = { w: 128, h: 80 };
const HAND = { x: 16, y: 22 };
const MENU_AT = { x: 676, y: 104 };
const WHOLE = { x: 32, y: 52, w: 616, h: 484 };
const ZOOM_AT = { x: 300, y: 300 };
const SNIPS = [
  { dimIn: 0.75, dragA: 1.95, dragB: 2.95, release: 3.0, from: [70, 198] },
  { dimIn: 11.1, dragA: 11.7, dragB: 12.28, release: 12.32, from: [392, 204] },
  { dimIn: 25.45, dragA: 26.0, dragB: 26.6, release: 26.65, from: [110, 230] },
];
const CLICKS = [8.22, 8.42, 17.85, 23.6, 28.9, 32.55];
const SHAKE = { a: 13.2, b: 14.6, release: 13.95, x: 720, y: 280 };
const PEN = { a: 36.9, b: 38.1, from: 200, sweep: 380, rx: 38, ry: 30 };
// Pin to screen: the menu opens at `at`, the card is pinned centred there, then dragged by
// about `drag` (pressed at `press`, let go at `release`), grown by two wheel notches (x1.1
// each, around the cursor) and closed with its close button (`CLOSE` px, `CLOSE_INSET` from
// the corner, as pins.rs).
const PIN = {
  at: { x: 680, y: 220 }, w: 256, h: 160, grow: 29.0,
  grab: { x: 620, y: 190 }, press: 30.0, release: 30.85, drag: { x: -70, y: 50 },
  wheel: [31.25, 31.65], step: 1.1, close: 32.55, CLOSE: 20, CLOSE_INSET: 8,
};
PIN.rect = { x: PIN.at.x - PIN.w / 2, y: PIN.at.y - PIN.h / 2, w: PIN.w, h: PIN.h, rot: 0 };
PIN.wheelAt = { x: PIN.grab.x + PIN.drag.x, y: PIN.grab.y + PIN.drag.y };
PIN.closeAt = (() => {
  const k = PIN.step * PIN.step, c = PIN.wheelAt;
  const x = c.x + (PIN.rect.x + PIN.drag.x - c.x) * k, y = c.y + (PIN.rect.y + PIN.drag.y - c.y) * k;
  return { x: x + PIN.w * k - PIN.CLOSE_INSET - PIN.CLOSE / 2, y: y + PIN.CLOSE_INSET + PIN.CLOSE / 2 };
})();

// Where the cursor is asked to be: [time, x, y], eased between.
const PATH = [
  [0, 760, 300],
  [0.95, 760, 300],
  [1.75, 70, 198],
  [1.95, 70, 198],
  [2.85, 390, 406],
  [3.05, 390, 406],
  [4.2, 720, 260],
  [6.75, 720, 260],
  [7.85, 190, 482],
  [9.3, 190, 482],
  [10.2, 260, 420],
  [11.0, 260, 420],
  [11.6, 392, 204],
  [11.7, 392, 204],
  [12.22, 608, 320],
  [12.4, 608, 320],
  [13.05, SHAKE.x, SHAKE.y],
  [15.9, SHAKE.x, SHAKE.y],
  [16.7, MENU_AT.x, MENU_AT.y],
  [18.1, MENU_AT.x, MENU_AT.y],
  [18.8, 905, 372],
  [22.75, 905, 372],
  [23.45, 828, 111],
  [25.0, 828, 111],
  [25.75, 110, 230],
  [26.0, 110, 230],
  [26.6, 350, 380],
  [26.75, 350, 380],
  [27.6, PIN.at.x, PIN.at.y],
  [28.4, PIN.at.x, PIN.at.y],
  [28.7, PIN.at.x + 70, PIN.at.y + 72],
  [29.05, PIN.at.x + 70, PIN.at.y + 72],
  [29.7, PIN.grab.x, PIN.grab.y],
  [PIN.press, PIN.grab.x, PIN.grab.y],
  [PIN.press + 0.6, PIN.wheelAt.x, PIN.wheelAt.y],
  [31.95, PIN.wheelAt.x, PIN.wheelAt.y],
  [32.4, PIN.closeAt.x, PIN.closeAt.y],
  [33.0, PIN.closeAt.x, PIN.closeAt.y],
  [33.8, ZOOM_AT.x, ZOOM_AT.y],
  [36.45, ZOOM_AT.x, ZOOM_AT.y],
  [PEN.a, ...penAt(0)],
  [PEN.b, ...penAt(1)],
  [39.3, ...penAt(1)],
  [40.5, 760, 300],
  [T, 760, 300],
];

const NOTE_TEXT = '# Trip plan\n- pack the tent\n- check the **weather**';
const TYPE_START = 18.75;
const TYPED_AT = (() => {
  const times = [];
  let at = TYPE_START;
  for (const ch of NOTE_TEXT) {
    times.push(at);
    at += ch === '\n' ? 0.32 : 0.066;
  }
  return times;
})();
const TYPE_END = TYPED_AT[TYPED_AT.length - 1];

function penAt(p) {
  const q = p * 0.7 + inOut(p) * 0.3;
  const a = ((PEN.from + PEN.sweep * q) * Math.PI) / 180;
  const wobble = 1 + 0.05 * Math.sin(a * 3);
  return [SUN.x + PEN.rx * wobble * Math.cos(a), SUN.y + PEN.ry * wobble * Math.sin(a)];
}

function pathAt(t) {
  let i = 0;
  while (i < PATH.length - 2 && t >= PATH[i + 1][0]) i++;
  const [ta, xa, ya] = PATH[i];
  const [tb, xb, yb] = PATH[i + 1];
  const e = inOut(ramp(t, ta, tb));
  return [mix(xa, xb, e), mix(ya, yb, e)];
}

function target(t) {
  if (t > SHAKE.a && t < SHAKE.b) {
    const p = (t - SHAKE.a) / (SHAKE.b - SHAKE.a);
    const env = inOut(ramp(p, 0, 0.18)) * inOut(1 - ramp(p, 0.78, 1));
    const w = 2 * Math.PI * 3.3 * (t - SHAKE.a);
    return [SHAKE.x + Math.sin(w) * 80 * env, SHAKE.y + Math.sin(w * 2) * 7 * env];
  }
  if (t > PEN.a && t < PEN.b) return penAt((t - PEN.a) / (PEN.b - PEN.a));
  return pathAt(t);
}

// The cursor follows its target on a spring (it arrives a little late), and a carried card
// follows the cursor on a softer one. Both are stepped once over the whole loop at a fixed
// rate, so any moment can be looked up and is always the same.
const STEP = 1 / 240;
const COUNT = Math.ceil(T / STEP) + 2;
const SIM = new Float64Array(COUNT * 8);
(function simulate() {
  let [x, y] = target(0);
  let vx = 0, vy = 0, cx = x, cy = y, cvx = 0, cvy = 0;
  const w1 = 2 * Math.PI * 2.7, z1 = 0.74;
  const w2 = 2 * Math.PI * 2.0, z2 = 0.56;
  for (let i = 0; i < COUNT; i++) {
    SIM.set([x, y, vx, vy, cx, cy, cvx, cvy], i * 8);
    const [tx, ty] = target(i * STEP);
    vx += (w1 * w1 * (tx - x) - 2 * z1 * w1 * vx) * STEP;
    vy += (w1 * w1 * (ty - y) - 2 * z1 * w1 * vy) * STEP;
    x += vx * STEP;
    y += vy * STEP;
    cvx += (w2 * w2 * (x - cx) - 2 * z2 * w2 * cvx) * STEP;
    cvy += (w2 * w2 * (y - cy) - 2 * z2 * w2 * cvy) * STEP;
    cx += cvx * STEP;
    cy += cvy * STEP;
  }
})();

function sim(t) {
  const f = clamp(t, 0, T) / STEP;
  const i = Math.min(Math.floor(f), COUNT - 2);
  const k = f - i;
  const out = new Array(8);
  for (let j = 0; j < 8; j++) out[j] = mix(SIM[i * 8 + j], SIM[(i + 1) * 8 + j], k);
  return { x: out[0], y: out[1], vx: out[2], vy: out[3], cx: out[4], cy: out[5], cvx: out[6], cvy: out[7] };
}

function hang(s) {
  return { x: s.cx + HAND.x, y: s.cy + HAND.y, w: CARD.w, h: CARD.h, rot: -5 + clamp(s.cvx * 0.014, -14, 14) };
}

function lerpRect(a, b, p) {
  return { x: mix(a.x, b.x, p), y: mix(a.y, b.y, p), w: mix(a.w, b.w, p), h: mix(a.h, b.h, p), rot: mix(a.rot || 0, b.rot || 0, p) };
}

function snipRect(snip, t) {
  const end = sim(Math.min(t, snip.dragB));
  const [fx, fy] = snip.from;
  return { x: Math.min(fx, end.x), y: Math.min(fy, end.y), w: Math.abs(end.x - fx), h: Math.abs(end.y - fy), rot: 0 };
}

// The pin's frame at t: centred where the menu opened, moved by the drag (it follows the
// cursor exactly), then grown by the wheel around the cursor.
function pinRect(t) {
  const a = sim(PIN.press), b = sim(clamp(t, PIN.press, PIN.release));
  const x = PIN.rect.x + b.x - a.x, y = PIN.rect.y + b.y - a.y;
  const k = Math.pow(PIN.step, inOut(ramp(t, PIN.wheel[0], PIN.wheel[0] + 0.12)) + inOut(ramp(t, PIN.wheel[1], PIN.wheel[1] + 0.12)));
  const c = PIN.wheelAt;
  return { x: c.x + (x - c.x) * k, y: c.y + (y - c.y) * k, w: PIN.w * k, h: PIN.h * k, rot: 0 };
}

function zoomRect(t) {
  const z = 1 + 0.5 * inOut(ramp(t, 35.3, 35.5)) + 0.5 * inOut(ramp(t, 35.65, 35.85));
  const w = WHOLE.w / z, h = WHOLE.h / z;
  return {
    z,
    x: clamp(ZOOM_AT.x - w / 2, WHOLE.x, WHOLE.x + WHOLE.w - w),
    y: clamp(ZOOM_AT.y - h / 2, WHOLE.y, WHOLE.y + WHOLE.h - h),
    w,
    h,
    rot: 0,
  };
}

// --- Markup helpers ------------------------------------------------------------

function keysHTML(spec) {
  return spec
    .split(/(\[[^\]]+\]|\{wheel\})/)
    .filter((part) => part.trim())
    .map((part) => {
      if (part === '{wheel}') return '<span class="wheel"></span>';
      if (part[0] === '[') return `<kbd>${part.slice(1, -1)}</kbd>`;
      return `<span class="word">${part.trim()}</span>`;
    })
    .join('');
}

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');

function noteHTML(count, caret) {
  const lines = NOTE_TEXT.slice(0, count).split('\n');
  return lines
    .map((line, i) => {
      let cls = 'p', body = line;
      if (line.startsWith('# ')) { cls = 'h'; body = line.slice(2); }
      else if (line.startsWith('- ')) { cls = 'li'; body = line.slice(2); }
      let html = esc(body).replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>');
      if (i === lines.length - 1) html += `<span class="tc" style="opacity:${caret ? 1 : 0}"></span>`;
      return `<div class="${cls}">${html || '&#8203;'}</div>`;
    })
    .join('');
}

// --- Rendering -------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const el = {
  screen: $('screen'), stage: $('stage'), cursor: $('cursor'), cross: $('cross'), pentip: $('pentip'),
  region: $('region'), snipbar: $('snipbar'), card1: $('card1'), card2: $('card2'), card3: $('card3'), card4: $('card4'),
  pin: $('pin'), pinX: $('pinX'), pinRow: $('pinRow'), menuRows: [...document.querySelectorAll('#menu .menu-row')],
  thumb: $('thumb'), placeholder: $('placeholder'), caret: $('caret'), menu: $('menu'), note: $('note'),
  noteBody: $('noteBody'), hideBtn: $('hideBtn'), trayLogo: $('trayLogo'), outline: $('outline'),
  strip: $('strip'), recTime: $('recTime'), recZoom: $('recZoom'), recPen: $('recPen'), ink: $('ink'),
  hud: $('hud'), ripples: [$('ripple1'), $('ripple2')],
  capTitle: $('capTitle'), capKeys: $('capKeys'), capText: $('capText'), dots: $('dots'),
};
const memo = {};
function setOnce(key, value, apply) {
  if (memo[key] !== value) {
    memo[key] = value;
    apply(value);
  }
}

function place(node, r, opacity) {
  node.style.opacity = opacity;
  if (opacity <= 0) return;
  node.style.width = r.w.toFixed(2) + 'px';
  node.style.height = r.h.toFixed(2) + 'px';
  node.style.transform = `translate(${r.x.toFixed(2)}px, ${r.y.toFixed(2)}px) rotate(${(r.rot || 0).toFixed(2)}deg)`;
}

let trayAt = null;
function trayPoint() {
  if (!trayAt) {
    const k = el.screen.getBoundingClientRect().width / 960 || 1;
    const s = el.screen.getBoundingClientRect();
    const r = el.trayLogo.getBoundingClientRect();
    trayAt = { x: (r.left + r.width / 2 - s.left) / k, y: (r.top + r.height / 2 - s.top) / k };
  }
  return trayAt;
}

function sceneAt(t) {
  return Math.max(0, SCENES.findIndex((s) => t >= s.start && t < s.end));
}

function renderAt(time) {
  const t = ((time % T) + T) % T;
  const s = sim(t);

  // Snipping: the screen dims, a box is dragged, and what it holds becomes a card.
  let dim = 0, region = null, snipping = false;
  for (const snip of SNIPS) {
    const d = ramp(t, snip.dimIn, snip.dimIn + 0.15) * (1 - ramp(t, snip.release, snip.release + 0.2));
    if (d > 0) {
      dim = d;
      snipping = t < snip.release;
      if (t >= snip.dragA) region = snipRect(snip, t);
    }
  }
  el.region.style.opacity = dim;
  if (dim > 0) {
    const r = region || { x: -20, y: -20, w: 0, h: 0 };
    el.region.style.outlineWidth = region ? '1px' : '0';
    place(el.region, r, dim);
  }
  el.snipbar.style.opacity = dim;
  el.snipbar.style.transform = `translate(-50%, ${(-10 * (1 - dim)).toFixed(2)}px)`;

  // Card 1: the first screenshot. Carried, then pasted into the comment box.
  const r1 = snipRect(SNIPS[0], SNIPS[0].dragB);
  if (t >= 3.0 && t < 9.1) {
    const carried = lerpRect(r1, hang(s), settle(ramp(t, 3.0, 3.55)));
    const q = settle(ramp(t, 8.55, 9.05));
    const r = lerpRect(carried, { ...THUMB, rot: 0 }, q);
    el.card1.style.padding = mix(3, 0.5, q) + 'px';
    place(el.card1, r, 1 - ramp(q, 0.9, 1));
  } else {
    el.card1.style.opacity = 0;
  }
  const pasted = ramp(t, 8.95, 9.05) * (1 - ramp(t, 41.9, 42.6));
  el.thumb.style.opacity = pasted;
  el.placeholder.style.opacity = 0.8 * (1 - ramp(t, 8.7, 8.9)) + 0.8 * ramp(t, 42.0, 42.6);
  const caretOn = t >= 9.05 && t < 41.9 && (t - 9.05) % 1 < 0.5;
  el.caret.style.opacity = caretOn ? 1 : 0;
  el.caret.style.left = '70px';

  // Card 2: the second screenshot. Shaken off: it drops a little, turns and fades.
  const r2 = snipRect(SNIPS[1], SNIPS[1].dragB);
  if (t >= SNIPS[1].release && t < SHAKE.release + 0.8) {
    if (t < SHAKE.release) {
      place(el.card2, lerpRect(r2, hang(s), settle(ramp(t, SNIPS[1].release, SNIPS[1].release + 0.55))), 1);
    } else {
      const from = hang(sim(SHAKE.release));
      const tau = t - SHAKE.release;
      const r = {
        x: from.x + 24 * tau,
        y: from.y + 46 * inOut(ramp(tau, 0, 0.7)) + 90 * tau * tau,
        w: CARD.w * (1 - 0.08 * ramp(tau, 0, 0.6)),
        h: CARD.h * (1 - 0.08 * ramp(tau, 0, 0.6)),
        rot: from.rot + 16 * tau,
      };
      place(el.card2, r, 1 - ramp(tau, 0.1, 0.65));
    }
  } else {
    el.card2.style.opacity = 0;
  }

  // Card 4: the third screenshot, carried, then pinned where the tool menu opened.
  const r4 = snipRect(SNIPS[2], SNIPS[2].dragB);
  const pinR = pinRect(t);
  if (t >= SNIPS[2].release && t < PIN.grow + 0.5) {
    const carried = lerpRect(r4, hang(s), settle(ramp(t, SNIPS[2].release, SNIPS[2].release + 0.55)));
    const q = settle(ramp(t, PIN.grow, PIN.grow + 0.45));
    el.card4.style.padding = mix(3, 0, q) + 'px';
    place(el.card4, lerpRect(carried, pinR, q), 1 - ramp(t, PIN.grow + 0.38, PIN.grow + 0.5));
  } else {
    el.card4.style.opacity = 0;
  }

  // The pin: shown once the card lands, follows the cursor while dragged, grows with the
  // wheel, and shows its close button while hovered.
  const pinOn = ramp(t, PIN.grow + 0.38, PIN.grow + 0.48) * (1 - ramp(t, PIN.close + 0.05, PIN.close + 0.3));
  if (pinOn > 0) {
    const shrink = 1 - 0.04 * ramp(t, PIN.close + 0.05, PIN.close + 0.3);
    const r = pinR;
    place(el.pin, { ...r, x: r.x + (r.w * (1 - shrink)) / 2, y: r.y + (r.h * (1 - shrink)) / 2, w: r.w * shrink, h: r.h * shrink }, pinOn);
    const over = s.x >= r.x && s.x <= r.x + r.w && s.y >= r.y && s.y <= r.y + r.h;
    el.pinX.style.opacity = over ? 1 : 0;
    const cx = r.x + r.w - PIN.CLOSE_INSET - PIN.CLOSE / 2, cy = r.y + PIN.CLOSE_INSET + PIN.CLOSE / 2;
    el.pinX.classList.toggle('hot', Math.hypot(s.x - cx, s.y - cy) <= PIN.CLOSE / 2 + 1);
  } else {
    el.pin.style.opacity = 0;
  }

  // Card 3: the finished video.
  const z = zoomRect(Math.min(t, 38.6));
  if (t >= 38.65) {
    const r = lerpRect(z, hang(s), settle(ramp(t, 38.65, 39.2)));
    place(el.card3, r, 1 - ramp(t, 41.9, 42.6));
  } else {
    el.card3.style.opacity = 0;
  }

  // Clicks leave a small ring.
  el.ripples.forEach((ring, n) => {
    const at = CLICKS.find((c, j) => j % 2 === n && t >= c && t < c + 0.45);
    if (at === undefined) {
      ring.style.opacity = 0;
      return;
    }
    const p = ramp(t, at, at + 0.45);
    const c = sim(at);
    ring.style.opacity = (0.55 * (1 - p)).toFixed(3);
    ring.style.transform = `translate(${c.x.toFixed(1)}px, ${c.y.toFixed(1)}px) scale(${(0.35 + 0.9 * p).toFixed(3)})`;
  });

  // The tool menu, its first row under the cursor. Pin to screen is offered only while a
  // card is carried, so the notes scene shows two rows and the pin scene three.
  const pinMenu = t >= 25;
  const [mIn, mOut, mAt] = pinMenu ? [28.2, 28.95, PIN.at] : [17.2, 18.0, MENU_AT];
  const menu = ramp(t, mIn, mIn + 0.12) * (1 - ramp(t, mOut, mOut + 0.1));
  el.menu.style.opacity = menu;
  if (menu > 0) {
    el.menu.style.transform = `translate(${mAt.x - 9}px, ${(mAt.y - 16 + 4 * (1 - ramp(t, mIn, mIn + 0.12))).toFixed(2)}px)`;
    // The row under the cursor is lit (36 px rows, the first starting 11 px above the menu's point).
    const row = pinMenu ? clamp(Math.floor((s.y - (mAt.y - 11)) / 36), 0, 2) : 0;
    setOnce('menu', pinMenu + '|' + row, () => {
      el.pinRow.style.display = pinMenu ? '' : 'none';
      el.menuRows.forEach((r, j) => r.classList.toggle('hot', j === row));
    });
  }

  // The note: grows from where New note was picked, is typed in, then hidden to the tray.
  const open = ramp(t, 18.05, 18.45);
  const gone = inOut(ramp(t, 23.72, 24.3));
  const noteOn = open > 0 && t < 24.3;
  el.note.style.opacity = noteOn ? Math.min(1, open * 3) * (1 - ramp(t, 23.9, 24.3)) : 0;
  if (noteOn) {
    const tray = trayPoint();
    const scale = (0.3 + 0.7 * settle(open)) * (1 - 0.94 * gone);
    const dx = (tray.x - MENU_AT.x) * gone, dy = (tray.y - MENU_AT.y) * gone;
    el.note.style.transform = `translate(${dx.toFixed(2)}px, ${dy.toFixed(2)}px) scale(${scale.toFixed(4)})`;
    let typed = 0;
    while (typed < TYPED_AT.length && TYPED_AT[typed] <= t) typed++;
    const caret = open >= 1 && (t < TYPE_END + 0.4 || (t - TYPE_END) % 1 < 0.5);
    setOnce('note', typed + '|' + caret, () => { el.noteBody.innerHTML = noteHTML(typed, caret); });
  }
  el.hideBtn.classList.toggle('hover', t > 23.35 && t < 23.8);
  const bump = span(t, 24.18, 24.6, 0.12, 0.3);
  el.trayLogo.style.transform = `scale(${(1 + 0.3 * bump).toFixed(3)})`;

  // Recording: the outline, the strip, zoom, the pen.
  const recOn = ramp(t, 34.25, 34.45);
  const recOff = ramp(t, 38.6, 38.8);
  const rec = recOn * (1 - recOff);
  place(el.outline, zoomRect(t), rec);
  el.strip.style.opacity = rec;
  if (rec > 0) {
    el.strip.style.transform = `translate(-50%, ${(-6 * (1 - recOn)).toFixed(2)}px)`;
    const secs = Math.min(9, Math.floor(Math.max(0, t - 34.3)));
    setOnce('rec', secs, (v) => { el.recTime.textContent = '0:0' + v; });
    const zoom = zoomRect(t).z;
    setOnce('zoom', zoom > 1.02 ? zoom.toFixed(1) + 'x' : '', (v) => {
      el.recZoom.textContent = v;
      el.recZoom.classList.toggle('show', !!v);
    });
    el.recPen.classList.toggle('on', t >= 36.4);
  }
  const inkAlpha = t >= PEN.a ? 1 - ramp(t, 38.6, 38.85) : 0;
  if (inkAlpha > 0) {
    const pts = [];
    const end = Math.min(t, PEN.b + 0.05);
    for (let u = PEN.a; u <= end; u += 1 / 90) {
      const p = sim(u);
      pts.push(p.x.toFixed(1) + ' ' + p.y.toFixed(1));
    }
    el.ink.setAttribute('d', pts.length > 1 ? 'M' + pts.join('L') : '');
  } else if (memo.ink !== 0) {
    el.ink.setAttribute('d', '');
  }
  memo.ink = inkAlpha;
  el.ink.style.opacity = inkAlpha;

  // Keys being pressed.
  const hud = HUD.find(([a, b]) => t >= a && t < b);
  setOnce('hud', hud ? hud[2] : '', (v) => { if (v) el.hud.innerHTML = keysHTML(v); });
  const hudIn = hud ? span(t, hud[0], hud[1], 0.12, 0.2) : 0;
  el.hud.style.opacity = hudIn;
  el.hud.style.transform = `translateY(${(6 * (1 - Math.min(1, hudIn))).toFixed(2)}px)`;

  // The pointer: crosshair while snipping, the pen's tip while drawing, else the arrow.
  const pen = t >= 36.42 && t < 38.62;
  const down = [[8.22, 8.3], [8.42, 8.5], [17.85, 17.95], [23.6, 23.7], [28.9, 28.98], [PIN.press, PIN.release], [32.55, 32.63]]
    .some(([a, b]) => t >= a && t < b);
  const arrowScale = (1 + 0.3 * rec) * (down ? 0.86 : 1);
  const lean = clamp(s.vx * 0.011, -12, 12);
  el.cursor.style.opacity = snipping || pen ? 0 : 1;
  el.cursor.style.transform = `translate(${s.x.toFixed(2)}px, ${s.y.toFixed(2)}px) rotate(${lean.toFixed(2)}deg) scale(${arrowScale.toFixed(3)})`;
  el.cross.style.opacity = snipping ? 1 : 0;
  el.cross.style.transform = `translate(${s.x.toFixed(2)}px, ${s.y.toFixed(2)}px)`;
  el.pentip.style.opacity = pen ? 1 : 0;
  el.pentip.style.transform = `translate(${s.x.toFixed(2)}px, ${s.y.toFixed(2)}px)`;

  // Caption and dots.
  const i = sceneAt(t);
  setOnce('scene', i, () => {
    const sc = SCENES[i];
    el.capTitle.textContent = sc.title;
    el.capKeys.innerHTML = keysHTML(sc.keys);
    el.capText.textContent = sc.text;
    [...el.dots.children].forEach((d, j) => {
      d.classList.toggle('on', j === i);
      d.setAttribute('aria-current', j === i ? 'step' : 'false');
    });
  });
  const sc = SCENES[i];
  const bar = el.dots.children[i] && el.dots.children[i].firstChild;
  if (bar) bar.style.width = (100 * ramp(t, sc.start, sc.end)).toFixed(2) + '%';
  [...el.dots.children].forEach((d, j) => { if (j !== i) d.firstChild.style.width = '0'; });
}

// --- Scale, playback, dots -------------------------------------------------------

function fit() {
  const w = el.stage.clientWidth, h = el.stage.clientHeight;
  if (!w || !h) return;
  el.screen.style.transform = `scale(${Math.min(w / 960, h / 600)})`;
  trayAt = null;
}
fit();
new ResizeObserver(fit).observe(el.stage);

SCENES.forEach((sc, i) => {
  const dot = document.createElement('button');
  dot.type = 'button';
  dot.className = 'dot';
  dot.setAttribute('aria-label', sc.title);
  dot.appendChild(document.createElement('i'));
  dot.addEventListener('click', () => play(i));
  el.dots.appendChild(dot);
});

const reduce = matchMedia('(prefers-reduced-motion: reduce)');
const HOLD = 2.5;
const startT = Number(params.get('t')) || 0;
let clock = { origin: performance.now(), from: startT };
let pin = null;
let frame = 0;

function now() {
  const ms = performance.now();
  if (pin) {
    const sc = SCENES[pin.scene];
    const elapsed = (ms - pin.at) / 1000, dur = sc.end - sc.start;
    if (elapsed < dur) return sc.start + elapsed;
    if (elapsed < dur + HOLD) return sc.end - 0.001;
    clock = { origin: pin.at + (dur + HOLD) * 1000, from: sc.end };
    pin = null;
  }
  return clock.from + (ms - clock.origin) / 1000;
}

function tick() {
  renderAt(now());
  frame = requestAnimationFrame(tick);
}

// A dot plays its scene from the start, holds its end briefly, then the loop goes on.
function play(i) {
  if (STILL || reduce.matches) {
    renderAt(SCENES[i].still);
    return;
  }
  pin = { scene: i, at: performance.now() };
  renderAt(now());
}

function start() {
  cancelAnimationFrame(frame);
  if (STILL) {
    renderAt(startT);
  } else if (reduce.matches) {
    renderAt(SCENES[sceneAt(((startT % T) + T) % T)].still);
  } else {
    clock = { origin: performance.now(), from: startT };
    tick();
  }
}
reduce.addEventListener('change', start);
start();

window.renderAt = renderAt;
window.DEMO = { duration: T, scenes: SCENES };
