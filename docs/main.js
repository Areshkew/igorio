'use strict';

// The "Pay what you want" page. The button stays hidden while this is empty.
const SUPPORT_URL = 'https://buy.polar.sh/polar_cl_fD77eebHRUazKLnDC7GmUVxaOvp0thmQb2wg04NjlfL';

const REPO = 'Areshkew/igorio';

// ?t=<seconds> starts the demo there; add &still=1 to show only that frame.
// ?record=1 shows only the demo and its caption, for recording (see tools/record.py).
// ?og=1 lays the page out as the 1200 x 630 Open Graph picture.
const params = new URLSearchParams(location.search);
const RECORD = params.has('record');
const OG = params.has('og');
const STILL = params.has('still') || RECORD;
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
  fetch('https://api.github.com/repos/' + REPO, { headers: { Accept: 'application/vnd.github+json' } })
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
    .then((data) => {
      const n = data && data.stargazers_count;
      if (typeof n !== 'number') return;
      const text = n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1).replace(/\.0$/, '') + 'k' : String(n);
      document.getElementById('star-count').textContent = text;
      document.getElementById('stars').hidden = false;
      document.getElementById('github').setAttribute('aria-label', `GitHub, ${n} stars`);
    })
    .catch(() => {});
})();

// ---------------------------------------------------------------------------
// The demo. Everything on the stage is a pure function of time: renderAt(t).

const T = 35;

const SCENES = [
  { title: 'Take a screenshot', keys: '[Win][Shift][S] or [PrtScn]', text: 'It turns into a small card that follows your cursor.', start: 0, end: 6.5, still: 4.6 },
  { title: 'Double-click to drop', keys: 'double-click', text: 'On a text box it pastes. On a folder or the desktop it saves a PNG.', start: 6.5, end: 11, still: 9.7 },
  { title: 'Shake to let go', keys: 'shake', text: 'Shake once to drop the top card. Keep shaking to drop them all.', start: 11, end: 16, still: 14.25 },
  { title: 'Notes', keys: 'hold [Alt]', text: 'Hold Alt anywhere except a text box, then pick New note. Notes take Markdown.', start: 16, end: 25, still: 22.8 },
  { title: 'Record the screen', keys: '[Ctrl][Shift][6]', text: 'Records the screen under the cursor to MP4. Zoom with Alt + scroll, draw with Ctrl+Shift+7.', start: 25, end: 35, still: 29.9 },
];

// Keys shown on the screen while they are pressed: [from, to, keys].
const HUD = [
  [0.45, 1.6, '[Win][Shift][S]'],
  [8.0, 8.8, 'double-click'],
  [11.0, 11.7, '[PrtScn]'],
  [13.2, 14.5, 'shake'],
  [16.85, 18.15, 'hold [Alt]'],
  [25.85, 26.65, '[Ctrl][Shift][6]'],
  [27.0, 28.05, '[Alt] + {wheel} scroll'],
  [28.2, 28.95, '[Ctrl][Shift][7]'],
  [30.3, 31.0, '[Ctrl][Shift][6]'],
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
];
const CLICKS = [8.22, 8.42, 17.85, 23.6];
const SHAKE = { a: 13.2, b: 14.6, release: 13.95, x: 720, y: 280 };
const PEN = { a: 28.9, b: 30.1, from: 200, sweep: 380, rx: 38, ry: 30 };

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
  [25.8, ZOOM_AT.x, ZOOM_AT.y],
  [28.45, ZOOM_AT.x, ZOOM_AT.y],
  [PEN.a, ...penAt(0)],
  [PEN.b, ...penAt(1)],
  [31.3, ...penAt(1)],
  [32.5, 760, 300],
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

function zoomRect(t) {
  const z = 1 + 0.5 * inOut(ramp(t, 27.3, 27.5)) + 0.5 * inOut(ramp(t, 27.65, 27.85));
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
  region: $('region'), snipbar: $('snipbar'), card1: $('card1'), card2: $('card2'), card3: $('card3'),
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
  const pasted = ramp(t, 8.95, 9.05) * (1 - ramp(t, 33.9, 34.6));
  el.thumb.style.opacity = pasted;
  el.placeholder.style.opacity = 0.8 * (1 - ramp(t, 8.7, 8.9)) + 0.8 * ramp(t, 34.0, 34.6);
  const caretOn = t >= 9.05 && t < 33.9 && (t - 9.05) % 1 < 0.5;
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

  // Card 3: the finished video.
  const z = zoomRect(Math.min(t, 30.6));
  if (t >= 30.65) {
    const r = lerpRect(z, hang(s), settle(ramp(t, 30.65, 31.2)));
    place(el.card3, r, 1 - ramp(t, 33.9, 34.6));
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

  // The tool menu, its first row under the cursor.
  const menu = ramp(t, 17.2, 17.32) * (1 - ramp(t, 18.0, 18.1));
  el.menu.style.opacity = menu;
  if (menu > 0) {
    el.menu.style.transform = `translate(${MENU_AT.x - 9}px, ${(MENU_AT.y - 16 + 4 * (1 - ramp(t, 17.2, 17.32))).toFixed(2)}px)`;
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
  const recOn = ramp(t, 26.25, 26.45);
  const recOff = ramp(t, 30.6, 30.8);
  const rec = recOn * (1 - recOff);
  place(el.outline, zoomRect(t), rec);
  el.strip.style.opacity = rec;
  if (rec > 0) {
    el.strip.style.transform = `translate(-50%, ${(-6 * (1 - recOn)).toFixed(2)}px)`;
    const secs = Math.min(9, Math.floor(Math.max(0, t - 26.3)));
    setOnce('rec', secs, (v) => { el.recTime.textContent = '0:0' + v; });
    const zoom = zoomRect(t).z;
    setOnce('zoom', zoom > 1.02 ? zoom.toFixed(1) + 'x' : '', (v) => {
      el.recZoom.textContent = v;
      el.recZoom.classList.toggle('show', !!v);
    });
    el.recPen.classList.toggle('on', t >= 28.4);
  }
  const inkAlpha = t >= PEN.a ? 1 - ramp(t, 30.6, 30.85) : 0;
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
  const pen = t >= 28.42 && t < 30.62;
  const down = [[8.22, 8.3], [8.42, 8.5], [17.85, 17.95], [23.6, 23.7]].some(([a, b]) => t >= a && t < b);
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
