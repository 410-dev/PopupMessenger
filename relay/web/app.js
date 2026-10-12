'use strict';
// PopupMessenger relay admin console ("Relay Manager").
// No framework: h() builds DOM nodes and every string goes in as a text node, so
// data from clients is never parsed as HTML. UI text is written in Korean and goes
// through t(), which looks it up in the EN dictionary (end of file) in English mode.

const $app = document.getElementById('app');

function h(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (k in e && typeof v !== 'string') e[k] = v;
    else if (k === 'value') e.value = v;
    else e.setAttribute(k, v === true ? '' : v);
  }
  for (const c of kids.flat(Infinity)) if (c != null && c !== false) e.append(c instanceof Node ? c : String(c));
  return e;
}

// ---------------------------------------------------------------- preferences
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
};

// ---------------------------------------------------------------- i18n
// Source strings are Korean; EN maps each one to English. "{name}" placeholders are
// filled from `p`. A key may end in "@@ctx" when one Korean word needs two English
// renderings (e.g. '정지' the button vs '정지@@state' the state); Korean drops it.
// To add a string: write t('한국어 {x}', { x }) in the code and add the key to EN.
const LANG_KEY = 'relay-lang';
let LANG = (() => {
  const s = store.get(LANG_KEY);
  if (s === 'ko' || s === 'en') return s;
  return /^ko/i.test(navigator.language || '') ? 'ko' : 'en';
})();
const MISSING = new Set(); // English strings not in EN yet (see window.relayI18n)
function t(key, p) {
  let s = key;
  if (LANG === 'en') {
    if (Object.prototype.hasOwnProperty.call(EN, key)) s = EN[key];
    else MISSING.add(key);
  }
  s = s.replace(/@@\w+$/, '');
  return p ? s.replace(/\{(\w+)\}/g, (m, k) => (k in p ? String(p[k]) : m)) : s;
}
window.relayI18n = { missing: MISSING };
const locale = () => (LANG === 'ko' ? 'ko-KR' : 'en-US');
document.documentElement.lang = LANG;

function setLang(l) {
  LANG = l;
  store.set(LANG_KEY, l);
  document.documentElement.lang = l;
  if (ME) shell(); else authScreen();
}
const langPick = () => h('select', { class: 'langpick', title: 'Language / 언어', onchange: (e) => setLang(e.target.value) },
  h('option', { value: 'ko', selected: LANG === 'ko' }, '한국어'),
  h('option', { value: 'en', selected: LANG === 'en' }, 'English'));

// Light / dark: follows the system unless picked (auto → light → dark).
const THEME_KEY = 'relay-theme';
let THEME = store.get(THEME_KEY) || 'auto';
function applyTheme() {
  if (THEME === 'light' || THEME === 'dark') document.documentElement.dataset.theme = THEME;
  else delete document.documentElement.dataset.theme;
}
applyTheme();
function themeBtn() {
  const glyph = { auto: '◐', light: '☀', dark: '☾' };
  const name = { auto: '테마: 시스템 설정', light: '테마: 밝게', dark: '테마: 어둡게' };
  const b = h('button', { class: 'tbtn', type: 'button' });
  const draw = () => { b.textContent = glyph[THEME] || glyph.auto; b.title = t(name[THEME] || name.auto); };
  b.addEventListener('click', () => {
    THEME = { auto: 'light', light: 'dark', dark: 'auto' }[THEME] || 'auto';
    store.set(THEME_KEY, THEME);
    applyTheme();
    draw();
  });
  draw();
  return b;
}

// ---------------------------------------------------------------- formatting
const pad = (n) => String(n).padStart(2, '0');
function ts(x) {
  if (!x) return '—';
  const d = new Date(x);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
function tsFull(x) {
  if (!x) return '';
  const d = new Date(x);
  return `${ts(x)}:${pad(d.getSeconds())}`;
}
function dur(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return t('{n}초', { n: s });
  const m = Math.floor(s / 60);
  if (m < 60) return t('{n}분', { n: m });
  const hr = Math.floor(m / 60);
  if (hr < 48) return t('{h}시간 {m}분', { h: hr, m: m % 60 });
  return t('{d}일 {h}시간', { d: Math.floor(hr / 24), h: hr % 24 });
}
function ago(x) {
  if (!x) return h('span', { class: 'mu' }, '—');
  const ms = Date.now() - x;
  return h('span', { title: tsFull(x) }, ms < 0 ? t('{d} 후', { d: dur(-ms) }) : ms < 60000 ? t('방금') : t('{d} 전', { d: dur(ms) }));
}
function until(x) {
  if (!x) return '—';
  const ms = x - Date.now();
  return h('span', { title: tsFull(x), class: ms < 3600000 ? 'err' : '' }, ms <= 0 ? t('만료됨') : t('{d} 남음', { d: dur(ms) }));
}
function bytes(n) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(n < 10 ? 2 : 1)} ${u[i]}` : `${n} B`;
}
const num = (n) => (Number(n) || 0).toLocaleString(locale());
const short = (id) => (id || '').slice(0, 8);
const clock = () => { const d = new Date(); return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`; };

// ---------------------------------------------------------------- api
// The tenancy the console works in: a tenancy admin's own, or the one a server
// admin picked ('' = the server itself).
let TEN = '';
// Appends the tenancy to a URL used outside api() (downloads).
const tq = (url) => (TEN && SESSION?.server_admin ? `${url}${url.includes('?') ? '&' : '?'}tenancy=${encodeURIComponent(TEN)}` : url);

async function api(method, path, body) {
  const opt = { method, credentials: 'same-origin', headers: { 'X-Relay-Admin': '1' } };
  if (TEN) opt.headers['X-Relay-Tenancy'] = TEN;
  if (body !== undefined) {
    opt.headers['Content-Type'] = 'application/json';
    opt.body = JSON.stringify(body);
  }
  const r = await fetch(path, opt);
  let j = {};
  try { j = await r.json(); } catch { /* empty body */ }
  if (r.status === 401 && !['/api/login', '/api/setup'].includes(path)) {
    authScreen();
    throw new Error(j.error || t('로그인이 필요합니다'));
  }
  if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
  return j;
}

// ---------------------------------------------------------------- toast / modal / drawer / menu
const toasts = h('div', { class: 'toasts' });
document.body.append(toasts);
function toast(msg, err) {
  const el = h('div', { class: 'toast' + (err ? ' er' : '') }, msg);
  toasts.append(el);
  setTimeout(() => el.remove(), err ? 6000 : 3000);
}
const fail = (e) => toast(String(e.message || e), true);
const copy = (text) => h('a', { class: 'lnk', onclick: () => navigator.clipboard.writeText(text).then(() => toast(t('복사했습니다'))) }, t('복사'));

function modal(title, body, { ok = t('확인'), danger = false, cancel = t('취소'), onOk, wide = false } = {}) {
  return new Promise((resolve) => {
    const err = h('div', { class: 'err' });
    const close = (v) => { scrim.remove(); box.remove(); document.removeEventListener('keydown', key); resolve(v); };
    const go = async () => {
      if (!onOk) return close(true);
      okBtn.disabled = true;
      try { const v = await onOk(); close(v === undefined ? true : v); } catch (e) { err.textContent = String(e.message || e); okBtn.disabled = false; }
    };
    const okBtn = h('button', { class: 'btn ' + (danger ? 'dan solid' : 'pri'), onclick: go }, ok);
    const key = (e) => { if (e.key === 'Escape') close(false); if (e.key === 'Enter' && e.target.tagName !== 'TEXTAREA') go(); };
    const scrim = h('div', { class: 'scrim', onclick: () => close(false) });
    const box = h('div', { class: 'modal' + (wide ? ' wide' : ''), role: 'dialog' },
      h('div', { class: 'hd' }, h('span', {}, title), h('span', { class: 'sp' }), h('button', { class: 'x', title: t('닫기'), onclick: () => close(false) }, '✕')),
      h('div', { class: 'bd' }, body, err),
      h('div', { class: 'ft' }, okBtn, cancel && h('button', { class: 'btn', onclick: () => close(false) }, cancel)));
    document.addEventListener('keydown', key);
    document.body.append(scrim, box);
    (box.querySelector('.bd input,.bd select,.bd textarea') || okBtn).focus();
  });
}
const confirmBox = (title, lines, ok = t('확인'), danger = false) =>
  modal(title, h('div', { class: 'lines' }, [].concat(lines).filter((l) => l !== '' && l != null).map((l) => (l instanceof Node ? l : h('div', {}, l)))), { ok, danger });

// The properties pane: slides in from the right over the page.
function drawer(title, body, foot) {
  const close = () => { scrim.remove(); d.remove(); document.removeEventListener('keydown', key); };
  const key = (e) => { if (e.key === 'Escape' && !document.querySelector('.modal')) close(); };
  const scrim = h('div', { class: 'scrim light', onclick: close });
  const d = h('div', { class: 'drawer' },
    h('div', { class: 'hd' }, h('span', { class: 'kind' }, t('속성')), h('h2', {}, title), h('span', { class: 'sp' }), h('button', { class: 'x', title: t('닫기'), onclick: close }, '✕')),
    h('div', { class: 'bd' }, body),
    foot && h('div', { class: 'ft' }, foot));
  document.addEventListener('keydown', key);
  document.body.append(scrim, d);
  return { close, el: d };
}

// A dropdown menu ("TASKS ▾"). items: [label, fn, { danger, disabled }] or '-'.
function closeMenus() { document.querySelectorAll('.menu.open').forEach((m) => m.classList.remove('open')); }
document.addEventListener('click', (e) => { if (!e.target.closest('.menu')) closeMenus(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeMenus(); });
function menu(label, items) {
  const list = h('div', { class: 'menu-list', role: 'menu' });
  const wrap = h('span', { class: 'menu' });
  const btn = h('button', {
    class: 'menu-btn', type: 'button',
    onclick: () => {
      const open = wrap.classList.contains('open');
      closeMenus();
      if (open) return;
      list.replaceChildren(...items.filter(Boolean).map((it) => (it === '-' ? h('div', { class: 'menu-sep' }) : h('button', {
        type: 'button', role: 'menuitem', class: 'menu-it' + (it[2]?.danger ? ' dan' : ''),
        disabled: typeof it[2]?.disabled === 'function' ? it[2].disabled() : !!it[2]?.disabled,
        onclick: () => { closeMenus(); it[1](); },
      }, it[0]))));
      wrap.classList.add('open');
    },
  }, label, h('span', { class: 'caret' }, '▾'));
  wrap.append(btn, list);
  return wrap;
}

// Form row in dialogs: label | control.
const field = (label, input) => h('div', { class: 'fld' }, h('label', {}, label), input);
// Two-column property list: [[label, value], …]; falsy entries are skipped.
const props = (pairs, cls = '') => h('div', { class: 'props ' + cls }, pairs.filter(Boolean).map(([k, v]) => h('div', { class: 'pr' }, h('div', { class: 'k' }, k), h('div', { class: 'v' }, v))));
// A content section: uppercase title, a subtitle, optional tools and a TASKS menu.
function sect(title, opt = {}, ...kids) {
  const sub = h('span', { class: 'sec-s' });
  const el = h('section', { class: 'sec' + (opt.cls ? ' ' + opt.cls : '') },
    h('div', { class: 'sec-hd' }, h('span', { class: 'sec-t' }, title), sub, h('span', { class: 'sp' }), opt.right, opt.tasks && menu(t('작업'), opt.tasks)),
    kids);
  el.sub = (...x) => sub.replaceChildren(...x.flat().filter((v) => v != null && v !== false && v !== ''));
  if (opt.sub) el.sub(opt.sub);
  return el;
}
// A Server Manager-style tile: a coloured band with the title and a count, then
// status lines. rows: [label, value, level ('wa' | 'er' | null)].
function tile(title, count, rows, href) {
  rows = rows.filter(Boolean);
  const lvl = rows.some((r) => r[2] === 'er') ? 'er' : rows.some((r) => r[2] === 'wa') ? 'wa' : 'ok';
  return h(href ? 'a' : 'div', { class: 'tile ' + lvl, href },
    h('div', { class: 'tile-hd' }, h('span', { class: 'tile-t' }, title), h('span', { class: 'tile-n' }, count)),
    h('div', { class: 'tile-bd' }, rows.map(([k, v, l]) => h('div', { class: 'tile-r' + (l ? ' ' + l : '') }, h('span', { class: 'mk' }), h('span', { class: 'k' }, k), h('span', { class: 'v' }, v)))));
}
const refreshNote = (s) => h('span', { class: 'refresh mu', title: t('이 화면이 열려 있는 동안 자동으로 갱신합니다') }, '⟳ ', t('{n}초마다 갱신', { n: s }));

// ---------------------------------------------------------------- data grid
const sortState = {};
// cols: { k, t, v(row) -> node|string, s(row) -> sort value, cls, sort: false }
// `order` ({ sort, asc, set(k) }) sorts on the server instead of in the page.
function grid(id, cols, rows, { key = 'id', sel, onSel, onRow, order, empty = t('항목이 없습니다') } = {}) {
  const st = order ? { k: order.sort, dir: order.asc ? 1 : -1 } : sortState[id] || (sortState[id] = { k: null, dir: 1 });
  const wrap = h('div', { class: 'tw' });
  function draw() {
    const list = rows.slice();
    const col = !order && cols.find((c) => c.k === st.k);
    if (col) {
      const sv = col.s || ((r) => r[col.k]);
      list.sort((a, b) => {
        const x = sv(a), y = sv(b);
        if (x == null && y == null) return 0;
        if (x == null) return 1;
        if (y == null) return -1;
        return (typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), LANG)) * st.dir;
      });
    }
    const head = h('tr', {},
      sel && h('th', { class: 'cb' }, h('input', {
        type: 'checkbox', title: t('모두 선택'),
        checked: list.length > 0 && list.every((r) => sel.has(r[key])),
        onchange: (e) => { list.forEach((r) => (e.target.checked ? sel.add(r[key]) : sel.delete(r[key]))); onSel && onSel(); draw(); },
      })),
      cols.map((c) => h('th', {
        class: [c.cls, c.sort !== false ? 's' : '', st.k === c.k ? 'on' : ''].filter(Boolean).join(' '),
        onclick: c.sort === false ? null : () => { if (order) return order.set(c.k); st.dir = st.k === c.k ? -st.dir : 1; st.k = c.k; draw(); },
      }, h('span', { class: 'th' }, c.t, c.sort !== false && h('span', { class: 'ar' }, st.k === c.k ? (st.dir > 0 ? '▲' : '▼') : '')))));
    const body = list.map((r) => h('tr', {
      class: [sel && sel.has(r[key]) ? 'sel' : '', onRow ? 'click' : ''].filter(Boolean).join(' '),
      onclick: onRow ? (e) => { if (!e.target.closest('input,button,a,select')) onRow(r); } : null,
    },
    sel && h('td', { class: 'cb' }, h('input', {
      type: 'checkbox', checked: sel.has(r[key]),
      onchange: (e) => { e.target.checked ? sel.add(r[key]) : sel.delete(r[key]); onSel && onSel(); draw(); },
    })),
    cols.map((c) => h('td', { class: c.cls || '' }, c.v ? c.v(r) : r[c.k] ?? ''))));
    wrap.replaceChildren(list.length
      ? h('table', { class: 'g' }, h('thead', {}, head), h('tbody', {}, body))
      : h('div', { class: 'empty' }, empty));
  }
  draw();
  return wrap;
}

// Paged list state: page, per, search text, filter and server-side sort.
function lister(path, opts = {}) {
  const st = { page: 1, per: opts.per || 100, q: '', filter: opts.filter || '', sort: opts.sort || null, asc: false, extra: {} };
  const L = {
    st,
    reload: null, // set by the page
    async fetch() {
      const qs = new URLSearchParams({ page: st.page, per: st.per, q: st.q, filter: st.filter, ...(st.sort ? { sort: st.sort, dir: st.asc ? 'asc' : 'desc' } : {}), ...st.extra });
      const r = await api('GET', `${path}?${qs}`);
      const pages = Math.max(1, Math.ceil(r.total / st.per));
      if (st.page > pages && r.total) { st.page = pages; return L.fetch(); }
      return r;
    },
    order() {
      return { sort: st.sort, asc: st.asc, set: (k) => { st.asc = st.sort === k ? !st.asc : false; st.sort = k; st.page = 1; L.reload(); } };
    },
    search(placeholder) {
      let tm = null;
      return h('span', { class: 'filter' }, h('input', { type: 'search', placeholder, title: placeholder, value: st.q, oninput: (e) => { clearTimeout(tm); tm = setTimeout(() => { st.q = e.target.value.trim(); st.page = 1; L.reload(); }, 300); } }));
    },
    pager(total) {
      return pagerBox(st.page, st.per, total, (n) => { st.page = n; L.reload(); }, (n) => { st.per = n; st.page = 1; L.reload(); });
    },
  };
  return L;
}
function pagerBox(page, per, total, go, setPer) {
  const pages = Math.max(1, Math.ceil(total / per));
  const to = (n) => () => go(Math.min(pages, Math.max(1, n)));
  const from = total ? (page - 1) * per + 1 : 0;
  return h('div', { class: 'pager' },
    h('span', { class: 'mu' }, t('{from}–{to} / {total}', { from: num(from), to: num(Math.min(total, page * per)), total: num(total) })),
    h('button', { class: 'pb', disabled: page <= 1, onclick: to(1), title: t('처음') }, '«'),
    h('button', { class: 'pb', disabled: page <= 1, onclick: to(page - 1), title: t('이전') }, '‹'),
    h('span', { class: 'pg' }, t('{page}/{pages}쪽', { page: num(page), pages: num(pages) })),
    h('button', { class: 'pb', disabled: page >= pages, onclick: to(page + 1), title: t('다음') }, '›'),
    h('button', { class: 'pb', disabled: page >= pages, onclick: to(pages), title: t('마지막') }, '»'),
    h('select', { title: t('쪽당 행 수'), onchange: (e) => setPer(Number(e.target.value)) },
      [50, 100, 200, 500].map((n) => h('option', { value: n, selected: n === per }, t('{n}개씩', { n })))));
}

const chip = (cls, text, title) => h('span', { class: 'chip ' + cls, title }, text);
// Labels are Korean source strings; translate where shown.
const STATE = { active: ['ok', '활성'], pending: ['wa', '승인 대기'], disabled: ['er', '정지@@state'] };
const stateChip = (s) => (STATE[s] ? chip(STATE[s][0], t(STATE[s][1])) : chip('ne', s));
const STATUS = { online: ['ok', '온라인'], dnd: ['wa', '방해금지'], offline: ['ne', '오프라인'] };
const statusChip = (s) => { const x = STATUS[s] || STATUS.online; return chip(x[0], t(x[1])); };
const ENC = { plain: '평문', tls: '서버 통신 암호화 (TLS)', e2ee: '종단간 암호화' };
const REG = { closed: '닫힘', approval: '승인제', open: '열림' };
const AUTH = { password: '사용자 이름 + 비밀번호', none: '없음 (기기 인증)' };
const TRANSPORT = { tcp: ['wa', 'TCP 평문'], tls: ['ok', 'TCP · TLS'], ws: ['wa', 'WebSocket 평문'], wss: ['ok', 'WebSocket · TLS'], 'ws-proxy': ['ok', 'WebSocket · 프록시 TLS'] };
const tr = (map, k) => (map[k] != null ? t(map[k]) : k);

function csv(name, header, rows) {
  const q = (v) => `"${String(v ?? '').replace(/"/g, '""')}"`;
  const text = '﻿' + [header, ...rows].map((r) => r.map(q).join(',')).join('\r\n');
  const a = h('a', { href: URL.createObjectURL(new Blob([text], { type: 'text/csv' })), download: name });
  document.body.append(a);
  a.click();
  setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
}

// ---------------------------------------------------------------- auth screens
let ME = null;
// { user, server_admin, tenancy: {id, name} | null, ... } from /api/session.
let SESSION = null;
let timer = null;
const TEN_KEY = 'relay-admin-tenancy';

async function authScreen() {
  clearInterval(timer);
  ME = null;
  TEN = '';
  document.title = t('릴레이 관리자');
  const s = await fetch('/api/session', { credentials: 'same-origin' }).then((r) => r.json());
  if (s.user) return start(s);
  const tenancy = h('input', { type: 'text', autocomplete: 'organization', placeholder: t('비우면 서버 총괄 관리자'), value: store.get('relay-admin-login-tenancy') || '' });
  const user = h('input', { type: 'text', autocomplete: 'username', placeholder: 'admin' });
  const pass = h('input', { type: 'password', autocomplete: s.setup ? 'new-password' : 'current-password' });
  const pass2 = h('input', { type: 'password', autocomplete: 'new-password' });
  const err = h('div', { class: 'err' });
  const btn = h('button', { class: 'btn pri' }, s.setup ? t('관리자 계정 만들기') : t('로그인'));
  async function go(e) {
    e.preventDefault();
    err.textContent = '';
    if (s.setup && pass.value !== pass2.value) { err.textContent = t('비밀번호가 서로 다릅니다'); return; }
    btn.disabled = true;
    try {
      const tn = tenancy.value.trim().toLowerCase();
      await api('POST', s.setup ? '/api/setup' : '/api/login', { username: user.value.trim(), password: pass.value, ...(s.setup ? {} : { tenancy: tn }) });
      store.set('relay-admin-login-tenancy', tn);
      start(await fetch('/api/session', { credentials: 'same-origin' }).then((r) => r.json()));
    } catch (x) { err.textContent = String(x.message || x); btn.disabled = false; }
  }
  $app.replaceChildren(h('div', { class: 'login' },
    h('div', { class: 'login-top' }, h('span', { class: 'brand' }, t('릴레이 관리자')), h('span', { class: 'sp' }), langPick(), themeBtn()),
    h('div', { class: 'login-mid' }, h('form', { class: 'dlg', onsubmit: go },
      h('div', { class: 'dlg-hd' }, s.setup ? t('최초 설정') : t('관리자 로그인')),
      h('div', { class: 'dlg-bd' },
        h('div', { class: 'dlg-title' }, h('div', { class: 'mu' }, 'PopupMessenger Relay', s.version ? ` · v${s.version}` : ''), h('div', { class: 'big' }, s.server_name)),
        s.setup && h('div', { class: 'warnbox' }, t('아직 관리자 계정이 없습니다. 지금 만드는 계정이 첫 서버 총괄 관리자가 됩니다. 이 화면은 첫 계정을 만든 뒤 다시 나타나지 않습니다.')),
        h('div', { class: 'form2' },
          !s.setup && field('Tenancy ID', tenancy),
          !s.setup && h('div', { class: 'help' }, t('Tenancy 관리자는 자기 Tenancy ID를 입력합니다. 서버 총괄 관리자는 비워 둡니다.')),
          field(t('관리자 이름'), user),
          field(s.setup ? t('비밀번호 ({n}자 이상)', { n: Math.max(8, s.min_password_len) }) : t('비밀번호'), pass),
          s.setup && field(t('비밀번호 확인'), pass2)),
        err),
      h('div', { class: 'dlg-ft' }, btn)))));
  (s.setup || tenancy.value ? user : tenancy).focus();
}

// Logged in: a tenancy admin works in their tenancy; a server admin in the
// tenancy picked last time, or on the server pages.
function start(s) {
  SESSION = s;
  ME = s.user;
  if (s.tenancy) TEN = s.tenancy.id;
  else TEN = store.get(TEN_KEY) || '';
  shell();
}

/// A server admin switches between the server and one tenancy.
function setTenancy(id) {
  TEN = id || '';
  store.set(TEN_KEY, TEN);
  location.hash = TEN ? '#/overview' : '#/server';
  shell();
}

// ---------------------------------------------------------------- shell & routing
// Pages within a tenancy: [id, title, page] or [group title].
const TENANT_PAGES = [
  ['운영'],
  ['overview', '대시보드', pOverview],
  ['connections', '접속 현황', pConnections],
  ['사용자'],
  ['accounts', '계정', pAccounts],
  ['departments', '부서', pDepartments],
  ['devices', '기기', pDevices],
  ['keys', 'API 키', pKeys],
  ['데이터'],
  ['messages', '보관함', pMessages],
  ['archive', '쪽지 감사', pArchive],
  ['audit', '감사 로그', pAudit],
  ['설정'],
  ['policy', '정책', pPolicy],
  ['admins', '관리자', pAdmins],
];
// A server admin's pages with no tenancy picked.
const SERVER_PAGES = [
  ['서버'],
  ['server', '대시보드', pServer],
  ['tenancies', 'Tenancy', pTenancies],
  ['server-settings', '서버 설정', pServerSettings],
  ['audit', '서버 감사 로그', pAudit],
  ['admins', '총괄 관리자', pAdmins],
];
const pages = () => (TEN ? TENANT_PAGES : SERVER_PAGES);
const navCounts = {};
let nav, main, crumbs, shellEl;
let TENANCIES = []; // server admins: the tenancy list, for names
let pageTitle = '';

function shell() {
  nav = h('nav', { class: 'side' });
  main = h('main', { class: 'main' });
  crumbs = h('div', { class: 'crumbs' });
  // Server admins pick where they work; tenancy admins see their tenancy.
  const where = SESSION.server_admin && h('select', { class: 'tenpick', title: t('작업할 곳'), onchange: (e) => setTenancy(e.target.value) },
    h('option', { value: '' }, t('서버 전체 (총괄)')));
  if (SESSION.server_admin) {
    api('GET', '/api/tenancies').then((r) => {
      TENANCIES = r.items;
      where.append(...r.items.map((x) => h('option', { value: x.id, selected: x.id === TEN }, `${x.name} (${x.id})`)));
      // The tenancy picked last time may have been deleted.
      if (TEN && !r.items.some((x) => x.id === TEN)) return setTenancy('');
      setCrumbs();
    }).catch(fail);
  }
  shellEl = h('div', { class: 'shell' },
    h('header', { class: 'top' },
      h('button', { class: 'tbtn navbtn', type: 'button', title: t('메뉴'), onclick: () => shellEl.classList.toggle('navopen') }, '☰'),
      crumbs, h('span', { class: 'sp' }),
      where && h('label', { class: 'twhere' }, h('span', { class: 'tl' }, t('작업 위치')), where),
      langPick(), themeBtn(),
      h('span', { class: 'who', title: SESSION.server_admin ? t('서버 총괄 관리자') : t('Tenancy 관리자') }, ME, h('span', { class: 'role' }, SESSION.server_admin ? t('총괄') : t('관리자'))),
      h('button', { class: 'tbtn', type: 'button', onclick: async () => { try { await api('POST', '/api/logout'); } catch { /* ignore */ } authScreen(); } }, t('로그아웃'))),
    nav, main, h('div', { class: 'navscrim', onclick: () => shellEl.classList.remove('navopen') }));
  $app.replaceChildren(shellEl);
  route();
  refreshCounts();
}

function setCrumbs(title) {
  if (title != null) pageTitle = title;
  const tn = TEN && (SESSION.tenancy || TENANCIES.find((x) => x.id === TEN) || { id: TEN, name: TEN });
  const segs = [
    SESSION.server_admin && TEN
      ? h('a', { class: 'c0', href: '#/server', onclick: (e) => { e.preventDefault(); setTenancy(''); }, title: t('서버 전체로 돌아가기') }, t('릴레이 관리자'))
      : h('a', { class: 'c0', href: TEN ? '#/overview' : '#/server' }, t('릴레이 관리자')),
    tn ? h('a', { href: '#/overview' }, t('Tenancy: {name}', { name: tn.name }), h('span', { class: 'cid' }, ` (${tn.id})`))
      : h('a', { href: '#/server' }, t('서버: {name}', { name: SESSION.server_name })),
    h('span', { class: 'cur' }, pageTitle),
  ];
  crumbs.replaceChildren(...segs.flatMap((s, i) => (i ? [h('span', { class: 'sep' }, '▸'), s] : [s])));
}

function drawNav(cur) {
  nav.replaceChildren(
    ...pages().map((p) => (p.length === 1
      ? h('div', { class: 'grp' }, t(p[0]))
      : h('a', { href: `#/${p[0]}`, class: p[0] === cur ? 'on' : '' }, h('span', { class: 'nl' }, t(p[1])),
        navCounts[p[0]] ? h('span', { class: 'cnt' + (p[0] === 'accounts' ? ' wa' : ''), title: p[0] === 'accounts' ? t('승인 대기') : t('접속 중') }, num(navCounts[p[0]])) : null))),
    h('div', { class: 'navfoot' }, h('div', {}, SESSION.server_name), h('div', { class: 'mono' }, SESSION.version ? `v${SESSION.version}` : '')));
}

async function refreshCounts() {
  if (!TEN) return drawNav(current());
  try {
    const o = await api('GET', '/api/overview');
    navCounts.accounts = o.counts.pending || 0;
    navCounts.connections = o.counts.online || 0;
    drawNav(current());
  } catch { /* shown elsewhere */ }
}

const current = () => (location.hash.replace(/^#\//, '') || (TEN ? 'overview' : 'server')).split('?')[0];

async function route() {
  if (!ME) return;
  clearInterval(timer);
  shellEl.classList.remove('navopen');
  const id = current();
  const page = pages().find((p) => p[0] === id) || pages()[1];
  drawNav(page[0]);
  setCrumbs(t(page[1]));
  document.title = `${t(page[1])} · ${t('릴레이 관리자')}`;
  const ph = h('div', { class: 'ph' });
  const content = h('div', { class: 'content' });
  main.replaceChildren(ph, content);
  try { await page[2](content, ph); } catch (e) { content.replaceChildren(h('div', { class: 'errbox' }, String(e.message || e))); }
}
window.addEventListener('hashchange', route);

/// Re-runs `fn` every `ms` while the page is current.
function every(ms, fn) {
  const id = current();
  timer = setInterval(() => { if (current() === id && !document.hidden && !document.querySelector('.modal,.drawer')) fn(); }, ms);
}

// ---------------------------------------------------------------- overview (tenancy dashboard)
async function pOverview(c, ph) {
  ph.append(h('span', { class: 'sub' }, t('이 Tenancy의 상태와 정책을 한눈에 봅니다')), h('span', { class: 'sp' }), refreshNote(5));
  const tiles = sect(t('상태'));
  const below = h('div', { class: 'stack' });
  c.append(tiles, below);
  let tileBox = null;
  async function draw() {
    const o = await api('GET', '/api/overview');
    const k = o.counts, s = o.server, p = o.policy, l = o.limits;
    navCounts.accounts = k.pending; navCounts.connections = k.online;
    drawNav('overview');
    const quota = l.storage_quota_mb ? k.stored_bytes / (l.storage_quota_mb * 1024 * 1024) : 0;
    tiles.sub(t('계정 {a} | 접속 {o} | 마지막 갱신 {t}', { a: num(k.accounts), o: num(k.online), t: clock() }));
    const box = h('div', { class: 'tiles' },
      tile(t('접속'), num(k.online), [
        [t('암호화된 연결'), num(k.online - k.online_plain)],
        [t('암호화 안 된 연결'), num(k.online_plain), k.online_plain ? 'wa' : null],
        [t('암호화'), tr(ENC, p.encryption), p.encryption === 'plain' ? 'wa' : null],
        [t('인증서'), s.tls_self_signed ? t('자체 서명') : t('운영자 제공')],
      ], '#/connections'),
      tile(t('계정'), num(k.accounts), [
        [t('활성'), num(k.active)],
        [t('승인 대기'), num(k.pending), k.pending ? 'wa' : null],
        [t('정지@@state'), num(k.disabled)],
        [t('외부인 가입'), tr(REG, p.registration), p.registration === 'open' ? 'wa' : null],
        l.max_accounts ? [t('사용자 수 한도'), `${num(k.accounts)} / ${num(l.max_accounts)}`, k.accounts >= l.max_accounts ? 'er' : null] : null,
      ], '#/accounts'),
      tile(t('기기'), num(k.devices), [
        [t('접속 중'), num(k.online)],
        [t('정지@@state'), num(k.devices_disabled)],
        [t('계정당 기기'), num(p.max_devices)],
      ], '#/devices'),
      tile(t('보관함'), num(k.messages), [
        [t('전달 대기 (기기 단위)'), num(k.pending_deliveries)],
        [t('가장 오래된 쪽지'), k.oldest_message ? ago(k.oldest_message) : '—'],
        [t('만료 알림 대기'), num(k.expired_notices)],
        [t('보관 시간'), t('쪽지 {a}시간 · 첨부 {b}시간', { a: p.retention_hours.text, b: p.retention_hours.files })],
      ], '#/messages'),
      tile(t('저장 공간'), bytes(k.stored_bytes), [
        [t('Tenancy 한도'), l.storage_quota_mb ? `${num(l.storage_quota_mb)} MB` : t('무제한')],
        l.storage_quota_mb ? [t('사용률'), `${(quota * 100).toFixed(1)}%`, quota > 0.95 ? 'er' : quota > 0.8 ? 'wa' : null] : null,
        [t('DB 크기'), bytes(s.db_bytes)],
      ]),
      tile(t('쪽지 감사'), num(k.archived), [
        [t('평문 쪽지 사본'), num(k.archived)],
      ], '#/archive'));
    if (tileBox) tileBox.replaceWith(box); else tiles.append(box);
    tileBox = box;
    const maxMsgs = Math.max(1, ...o.stats.map((x) => x.msgs));
    below.replaceChildren(
      h('div', { class: 'cols' },
        sect(t('서버 속성'), { sub: s.name }, props([
          [t('서버 이름'), s.name],
          [t('서버 ID'), h('span', { class: 'mono' }, s.id)],
          [t('버전'), s.version],
          [t('가동 시간'), h('span', { title: tsFull(s.started) }, dur(s.now - s.started))],
          [t('사용자 접속 주소'), s.public_url
            ? [h('span', { class: 'mono' }, s.public_url), ' ', copy(s.public_url)]
            : h('span', { class: 'mu' }, t('서버 설정 > 외부 공개에서 정할 수 있습니다 (총괄 관리자)'))],
          [t('릴레이 포트'), t('{port} (TLS 1.3 · 평문 TCP{ws} 자동 판별)', { port: s.relay_port, ws: s.websocket ? ' · WebSocket' : '' })],
          s.websocket && ['WebSocket', t('{n}KB 단위 스트리밍', { n: s.ws_chunk_kb }) + (s.proxy_tls ? t(' · 프록시가 TLS 처리') : '')],
          [t('인증서'), s.tls_self_signed ? chip('wa', t('자체 서명')) : chip('ok', t('운영자 제공'))],
          [t('SHA-256 지문'), [h('span', { class: 'mono' }, s.tls_fingerprint.match(/../g).join(':')), ' ', copy(s.tls_fingerprint)]],
          [t('데이터 경로'), h('span', { class: 'mono' }, s.data_dir)],
        ])),
        sect(t('현재 정책'), { right: h('a', { class: 'lnk', href: '#/policy' }, t('변경')) }, props([
          [t('로그온'), tr(AUTH, p.auth)],
          [t('외부인 가입'), tr(REG, p.registration)],
          [t('계정당 기기'), t('{n}대', { n: num(p.max_devices) })],
          [t('암호화'), tr(ENC, p.encryption)],
          [t('경유 강제'), p.force_relay ? (p.block_direct_outsiders ? t('예 · 외부 LAN 직접 전송 금지') : t('예')) : t('아니요')],
          [t('첨부 한도'), p.relay_max_files ? t('{n}개 · 파일당 {s}', { n: num(p.relay_max_files), s: bytes(p.relay_max_file_size) }) : t('첨부 중계 안 함')],
          [t('보관'), t('쪽지 {a}시간 · 첨부 {b}시간', { a: p.retention_hours.text, b: p.retention_hours.files })],
          [t('연락처'), p.contacts === 'all' ? t('전체 공개') : t('검색한 사용자만')],
        ]))),
      sect(t('Tenancy 속성'), { sub: o.tenancy.name }, props([
        ['Tenancy ID', [h('span', { class: 'mono' }, o.tenancy.id), ' ', copy(o.tenancy.id)]],
        [t('사용자 안내'), t('클라이언트에서 릴레이 서버 주소와 함께 이 Tenancy ID를 입력합니다')],
        [t('주소 형식'), h('span', { class: 'mono' }, `<${t('사용자 이름')}>@${o.tenancy.id}:${s.domain || `<${t('서버')}>`}`)],
        o.tenancy.unmigrated && ['Unmigrated', t('Tenancy를 지원하지 않는 구형 클라이언트와 기존 사용자가 들어오는 곳입니다. 계정 화면에서 사용자를 다른 Tenancy로 옮길 수 있습니다(총괄 관리자)')],
        [t('저장 공간'), l.storage_quota_mb ? `${bytes(k.stored_bytes)} / ${num(l.storage_quota_mb)} MB` : t('{v} (한도 없음)', { v: bytes(k.stored_bytes) })],
        [t('사용자 수 한도'), l.max_accounts ? t('{a} / {b}명', { a: num(k.accounts), b: num(l.max_accounts) }) : t('없음')],
        [t('파일 크기 상한'), l.max_file_size_mb ? `${num(l.max_file_size_mb)} MB` : t('없음')],
        [t('본문 상한'), `${num(l.max_body_kb)} KB`],
        ['', h('span', { class: 'mu' }, t('자원 한도는 서버 총괄 관리자가 정합니다'))],
      ])),
      sect(t('일별 처리량'), { sub: t('UTC, 최근 30일') },
        grid('stats', [
          { k: 'day', t: t('날짜'), cls: 'mono' },
          { k: 'msgs', t: t('쪽지'), cls: 'num', v: (r) => num(r.msgs) },
          { k: 'bar', t: '', sort: false, v: (r) => { const b = h('div', { class: 'bar' }); b.style.width = `${(r.msgs / maxMsgs) * 100}%`; return h('div', { class: 'bartrack' }, b); } },
          { k: 'deliveries', t: t('기기 전달'), cls: 'num', v: (r) => num(r.deliveries) },
          { k: 'bytes', t: t('첨부 용량'), cls: 'num', v: (r) => bytes(r.bytes) },
          { k: 'expired', t: t('만료 (기기 단위)'), cls: 'num', v: (r) => num(r.expired) },
        ], o.stats, { key: 'day', empty: t('아직 기록이 없습니다') })));
  }
  await draw();
  every(5000, () => draw().catch(fail));
}

// ---------------------------------------------------------------- connections
async function pConnections(c, ph) {
  const L = lister('/api/connections', { sort: 'since' });
  ph.append(h('span', { class: 'sub' }, t('지금 릴레이에 접속해 있는 기기')), h('span', { class: 'sp' }), refreshNote(3));
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const pg = h('div');
  const sec = sect(t('접속 중인 기기'), { tasks: [[t('새로 고침'), () => L.reload()]] },
    h('div', { class: 'tb' }, L.search(t('필터: 이름, 사용자 이름, 계정·기기 ID, IP, 앱')), h('span', { class: 'sp' }), pg), host, foot);
  async function load() {
    const r = await L.fetch();
    host.replaceChildren(grid('conns', [
      { k: 'name', t: t('이름'), sort: false, v: (x) => x.name || h('span', { class: 'mu' }, t('(이름 없음)')) },
      { k: 'username', t: t('사용자 이름'), sort: false, v: (x) => x.username || h('span', { class: 'mu' }, '—') },
      { k: 'status', t: t('상태'), sort: false, v: (x) => statusChip(x.status) },
      { k: 'device', t: t('기기 ID'), cls: 'mono' },
      { k: 'account', t: t('계정 ID'), cls: 'mono' },
      { k: 'ip', t: 'IP', cls: 'mono' },
      { k: 'transport', t: t('연결'), v: (x) => { const v = TRANSPORT[x.transport] || (x.tls ? ['ok', 'TLS'] : ['wa', '평문']); return chip(v[0], t(v[1])); } },
      { k: 'app', t: t('앱'), cls: 'mono' },
      { k: 'since', t: t('접속 시각'), v: (x) => h('span', { title: tsFull(x.since) }, ts(x.since)) },
      { k: 'dur', t: t('접속 시간'), cls: 'num', v: (x) => dur(Date.now() - x.since) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (x) => h('button', { class: 'btn sm', onclick: () => kick(x.device, x.name) }, t('끊기')) },
    ], r.items, { key: 'device', order: L.order(), empty: L.st.q ? t('조건에 맞는 연결이 없습니다') : t('접속 중인 기기가 없습니다') }));
    pg.replaceChildren(L.pager(r.total));
    sec.sub(t('전체 {n}개 | 암호화 안 됨 {p}개', { n: num(r.all), p: num(r.plain) }));
    foot.replaceChildren(h('span', {}, t('전체 {n}개 연결 · 암호화 안 된 연결 {p}개', { n: num(r.all), p: num(r.plain) })), h('span', {}, t('마지막 갱신 {t}', { t: clock() })));
  }
  L.reload = () => load().catch(fail);
  async function kick(id, name) {
    if (!(await confirmBox(t('연결 끊기'), [t('{name} 기기의 연결을 끊을까요?', { name: name || id }), t('클라이언트는 곧바로 다시 접속할 수 있습니다. 막으려면 기기를 정지하세요.')], t('끊기'), true))) return;
    try { await api('POST', `/api/devices/${encodeURIComponent(id)}/kick`); toast(t('연결을 끊었습니다')); load(); } catch (e) { fail(e); }
  }
  c.append(sec);
  await load();
  every(3000, () => load().catch(fail));
}

// ---------------------------------------------------------------- accounts
// Bulk actions on accounts: button, confirmation, result.
const BULK = {
  approve: { b: '승인', title: '계정 승인', q: '선택한 {n}개 계정을 승인할까요?', done: '{n}개 계정을 승인했습니다' },
  enable: { b: '정지 해제', title: '계정 정지 해제', q: '선택한 {n}개 계정의 정지를 해제할까요?', done: '{n}개 계정의 정지를 해제했습니다' },
  disable: { b: '정지', title: '계정 정지', q: '선택한 {n}개 계정을 정지할까요?', done: '{n}개 계정을 정지했습니다', danger: true },
  delete: { b: '삭제', title: '계정 삭제', q: '선택한 {n}개 계정을 삭제할까요?', done: '{n}개 계정을 삭제했습니다', danger: true },
};

async function pAccounts(c, ph) {
  const { settings } = await api('GET', '/api/settings');
  const deps = (await api('GET', '/api/departments')).items;
  const depName = (id) => (deps.find((d) => d.id === id) || {}).name || id;
  const pol = settings.policy;
  const hashQ = new URLSearchParams(location.hash.split('?')[1] || '');
  const L = lister('/api/accounts', { filter: hashQ.get('state') || '' });
  if (hashQ.get('department')) L.st.extra.department = hashQ.get('department');
  const depPick = deps.length > 0 && h('select', { title: t('부서'), onchange: (e) => { if (e.target.value) L.st.extra.department = e.target.value; else delete L.st.extra.department; L.st.page = 1; L.reload(); } },
    h('option', { value: '' }, t('모든 부서')), deps.map((d) => h('option', { value: d.id, selected: d.id === L.st.extra.department }, `${d.name} (${d.id})`)));
  // Server admins move users out of the unmigrated tenancy.
  const canMove = SESSION.server_admin && TEN === 'unmigrated';
  let rows = [];
  let counts = {};
  const sel = new Set();
  const seg = h('div', { class: 'seg' });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const pg = h('div');
  const bulkBtns = Object.entries(BULK).map(([a, x]) => h('button', { class: 'btn' + (x.danger ? ' dan' : ''), disabled: true, onclick: () => bulk(a) }, t(x.b)));
  if (canMove) bulkBtns.push(h('button', { class: 'btn', disabled: true, onclick: () => move() }, t('다른 Tenancy로 옮기기')));
  const selInfo = h('span', { class: 'selinfo' });
  if (pol.auth === 'none') ph.append(h('span', { class: 'sub' }, t('로그온 방식이 "없음"이라 기기 하나가 계정 하나입니다')));
  const sec = sect(t('계정'), {
    tasks: [[t('새 계정…'), create], '-', [t('새로 고침'), () => L.reload()]],
  },
  h('div', { class: 'tb' }, h('button', { class: 'btn pri', onclick: create }, t('새 계정')), h('span', { class: 'tsep' }), seg, depPick, L.search(t('필터: 사용자 이름, 표시 이름, ID, 메모'))),
  h('div', { class: 'tb sub' }, selInfo, bulkBtns, h('span', { class: 'sp' }), pg),
  host, foot);

  async function reload() {
    const r = await L.fetch();
    rows = r.items;
    counts = r.counts;
    // Opened without a filter while sign-ups wait: show those first.
    if (L.st.filter === '' && !hashQ.get('state') && counts.pending && !reload.done) { reload.done = true; L.st.filter = 'pending'; return reload(); }
    reload.done = true;
    sel.forEach((id) => { if (!rows.some((x) => x.id === id)) sel.delete(id); });
    draw(r.total);
    refreshCounts();
  }
  L.reload = () => reload().catch(fail);
  function onSel() {
    bulkBtns.forEach((b) => { b.disabled = !sel.size; });
    selInfo.textContent = sel.size ? t('{n}개 선택', { n: num(sel.size) }) : t('선택한 계정에 적용:');
  }
  function draw(total) {
    const f = L.st.filter || 'all';
    seg.replaceChildren(...[['all', t('전체 {n}', { n: num(counts.all) })], ['active', t('활성 {n}', { n: num(counts.active) })], ['pending', t('승인 대기 {n}', { n: num(counts.pending) })], ['disabled', t('정지 {n}', { n: num(counts.disabled) })]]
      .map(([k, x]) => h('button', { class: f === k ? 'on' : '', onclick: () => { L.st.filter = k === 'all' ? '' : k; L.st.page = 1; sel.clear(); L.reload(); } }, x)));
    host.replaceChildren(grid('accounts', [
      { k: 'state', t: t('상태'), v: (r) => stateChip(r.state) },
      { k: 'username', t: t('사용자 이름'), v: (r) => r.username || h('span', { class: 'mu' }, '—') },
      { k: 'name', t: t('표시 이름') },
      { k: 'departments', t: t('부서'), sort: false, cls: 'trunc', v: (r) => (r.departments.length ? r.departments.map(depName).join(', ') : h('span', { class: 'mu' }, '—')) },
      { k: 'id', t: t('계정 ID'), cls: 'mono' },
      { k: 'devices', t: t('기기'), cls: 'num', v: (r) => `${r.devices} / ${r.max_devices ?? pol.max_devices}${r.max_devices != null ? '*' : ''}` },
      { k: 'online', t: t('접속'), cls: 'num', sort: false, v: (r) => (r.online ? h('span', {}, h('span', { class: 'dot ok' }), ` ${r.online}`) : h('span', { class: 'mu' }, '0')) },
      { k: 'pending', t: t('받을 쪽지'), cls: 'num', v: (r) => (r.pending ? num(r.pending) : h('span', { class: 'mu' }, '0')) },
      { k: 'stored_bytes', t: t('보낸 첨부 보관'), cls: 'num', v: (r) => (r.stored_bytes ? bytes(r.stored_bytes) : h('span', { class: 'mu' }, '—')) },
      { k: 'quota_mb', t: t('저장 한도'), cls: 'num', v: (r) => (r.quota_mb != null ? `${num(r.quota_mb)} MB*` : h('span', { class: 'mu' }, settings.account_quota_mb ? `${num(settings.account_quota_mb)} MB` : t('무제한'))) },
      { k: 'last_seen', t: t('마지막 접속'), v: (r) => ago(r.last_seen) },
      { k: 'created', t: t('생성'), v: (r) => h('span', { title: tsFull(r.created) }, ts(r.created)) },
      { k: 'note', t: t('메모'), cls: 'trunc', v: (r) => h('span', { title: r.note }, r.note) },
    ], rows, { sel, onSel, onRow: open, order: L.order(), empty: f === 'pending' ? t('승인을 기다리는 가입 신청이 없습니다') : t('계정이 없습니다') }));
    pg.replaceChildren(L.pager(total));
    sec.sub(t('{f} | 조건에 맞는 {n}개', { f: { all: t('전체 계정'), active: t('활성'), pending: t('승인 대기'), disabled: t('정지@@state') }[f], n: num(total) }));
    foot.replaceChildren(h('span', {}, t('조건에 맞는 {n}개', { n: num(total) })), h('span', {}, t('* 계정별로 따로 정한 값')), h('span', {}, t('선택은 현재 쪽에만 적용됩니다')));
    onSel();
  }

  async function bulk(action) {
    const x = BULK[action];
    const ids = [...sel];
    const names = rows.filter((r) => sel.has(r.id)).map((r) => r.username || r.name || r.id);
    const lines = [t(x.q, { n: ids.length }), h('div', { class: 'mu' }, names.slice(0, 12).join(', ') + (names.length > 12 ? t(' 외 {n}개', { n: names.length - 12 }) : ''))];
    if (action === 'delete') lines.push(h('div', { class: 'warnbox' }, t('계정과 등록된 기기, 받지 못한 쪽지가 모두 삭제됩니다. 되돌릴 수 없습니다.')));
    if (action === 'disable') lines.push(t('접속 중인 기기는 곧바로 연결이 끊깁니다.'));
    if (!(await confirmBox(t(x.title), lines, t(x.b), !!x.danger))) return;
    try {
      const r = await api('POST', '/api/accounts/bulk', { ids, action });
      toast(t(x.done, { n: r.count }));
      sel.clear();
      reload();
    } catch (e) { fail(e); }
  }

  async function move() {
    const ids = [...sel];
    const targets = (await api('GET', '/api/tenancies')).items.filter((x) => !x.unmigrated);
    if (!targets.length) return toast(t('먼저 Tenancy를 만드세요'), true);
    const to = h('select', {}, targets.map((x) => h('option', { value: x.id }, `${x.name} (${x.id})`)));
    let r = null;
    const ok = await modal(t('다른 Tenancy로 옮기기'), [
      h('div', {}, t('선택한 {n}개 계정을 옮깁니다.', { n: ids.length })),
      field(t('옮길 Tenancy'), to),
      h('div', { class: 'help' }, t('계정, 등록된 기기(기기 비밀값 포함), 아직 받지 못한 쪽지와 첨부파일이 함께 옮겨집니다. 구형 클라이언트는 그대로 접속해도 새 Tenancy로 연결되고, 신형 클라이언트는 저장된 Tenancy ID를 자동으로 바꿉니다.')),
      h('div', { class: 'warnbox' }, t('옮긴 계정은 이 Tenancy의 연락처에서 사라집니다. 같은 사용자 이름이나 기기가 이미 있는 계정은 옮기지 않습니다.')),
    ], { ok: t('옮기기'), onOk: async () => { r = await api('POST', '/api/accounts/bulk', { ids, action: 'move', tenancy: to.value }); } });
    if (!ok || !r) return;
    toast(t('{n}개 계정을 {to} Tenancy로 옮겼습니다', { n: r.count, to: to.value }));
    if (r.failed.length) await confirmBox(t('옮기지 못한 계정'), r.failed.map((f) => `${(rows.find((x) => x.id === f.id) || {}).username || f.id}: ${f.error}`), t('확인'));
    sel.clear();
    reload();
  }

  async function create() {
    const f = {
      username: h('input', { type: 'text', autocomplete: 'off' }),
      password: h('input', { type: 'password', autocomplete: 'new-password' }),
      name: h('input', { type: 'text', placeholder: t('비우면 사용자 이름') }),
      max_devices: h('input', { type: 'number', min: 1, max: 9999, placeholder: t('정책 기본값 ({n})', { n: pol.max_devices }) }),
      quota_mb: h('input', { type: 'number', min: 0, placeholder: t('정책 기본값') }),
      note: h('input', { type: 'text' }),
    };
    const ok = await modal(t('새 계정'), [
      pol.auth === 'none' && h('div', { class: 'warnbox' }, t('지금 로그온 방식은 "없음"입니다. 사용자 이름으로 만든 계정은 로그온 방식이 "비밀번호"일 때 쓸 수 있습니다.')),
      field(t('사용자 이름'), f.username), field(t('비밀번호 ({n}자 이상)', { n: settings.min_password_len }), f.password), field(t('표시 이름'), f.name),
      field(t('기기 수 한도'), f.max_devices), field(t('저장 한도 (MB)'), f.quota_mb), field(t('메모'), f.note),
    ], {
      ok: t('만들기'),
      onOk: () => api('POST', '/api/accounts', {
        username: f.username.value.trim(), password: f.password.value, name: f.name.value.trim(), note: f.note.value,
        max_devices: f.max_devices.value === '' ? null : Number(f.max_devices.value),
        quota_mb: f.quota_mb.value === '' ? null : Number(f.quota_mb.value),
      }),
    });
    if (ok) { toast(t('계정을 만들었습니다')); reload(); }
  }

  async function open(a) {
    const devs = (await api('GET', `/api/devices?account=${encodeURIComponent(a.id)}&per=500`)).items;
    const f = {
      state: h('select', {}, Object.entries(STATE).map(([k, [, x]]) => h('option', { value: k, selected: a.state === k }, t(x)))),
      username: h('input', { type: 'text', value: a.username || '', placeholder: t('없음') }),
      name: h('input', { type: 'text', value: a.name }),
      password: h('input', { type: 'password', autocomplete: 'new-password', placeholder: t('바꿀 때만 입력') }),
      max_devices: h('input', { type: 'number', min: 1, max: 9999, value: a.max_devices ?? '', placeholder: t('정책 기본값 ({n})', { n: pol.max_devices }) }),
      quota_mb: h('input', { type: 'number', min: 0, value: a.quota_mb ?? '', placeholder: settings.account_quota_mb ? t('정책 기본값 ({n})', { n: settings.account_quota_mb }) : t('정책 기본값 (무제한)') }),
      note: h('textarea', { rows: 3, value: a.note }),
    };
    const depBoxes = deps.map((d) => h('input', { type: 'checkbox', value: d.id, checked: a.departments.includes(d.id) }));
    const depCtl = deps.length
      ? h('div', { class: 'checks' }, deps.map((d, i) => h('label', { class: 'line' }, depBoxes[i], `${d.name} (${d.id})`)))
      : h('span', { class: 'mu' }, t('부서가 없습니다. 부서 화면에서 만들 수 있습니다'));
    const row = (label, ctl, help) => h('div', { class: 'row' }, h('label', {}, label), h('div', { class: 'ctl' }, ctl, help && h('div', { class: 'help' }, help)));
    const devGrid = grid('acc-devs', [
      { k: 'online', t: '', v: (d) => h('span', { class: 'dot' + (d.online ? ' ok' : '') }), s: (d) => (d.online ? 0 : 1) },
      { k: 'name', t: t('이름'), s: (d) => d.profile.name, v: (d) => d.profile.name || '—' },
      { k: 'id', t: t('기기 ID'), cls: 'mono' },
      { k: 'app', t: t('앱'), cls: 'mono' },
      { k: 'last_seen', t: t('마지막 접속'), v: (d) => ago(d.last_seen) },
      { k: 'disabled', t: '', v: (d) => (d.disabled ? chip('er', t('정지@@state')) : '') },
    ], devs, { empty: t('등록된 기기가 없습니다') });
    const dr = drawer(a.username || a.name || a.id, [
      h('div', { class: 'pane-hd' }, h('span', {}, stateChip(a.state)), h('span', { class: 'mono mu' }, a.id)),
      props([
        [t('계정 ID'), h('span', { class: 'mono' }, a.id)],
        [t('생성'), tsFull(a.created)],
        [t('마지막 접속'), a.last_seen ? tsFull(a.last_seen) : '—'],
        [t('받을 쪽지'), t('{n}건 (기기 단위)', { n: num(a.pending) })],
        [t('비밀번호'), a.has_password ? t('설정됨') : t('없음')],
      ], 'one'),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('계정 정보')),
        row(t('상태'), f.state, t('정지하거나 승인 대기로 돌리면 접속 중인 기기의 연결이 끊깁니다')),
        row(t('사용자 이름'), f.username, t('비우면 비밀번호 로그온을 쓸 수 없습니다')),
        row(t('표시 이름'), f.name, t('기기가 접속하면 기기에서 보낸 이름으로 바뀝니다')),
        row(t('부서'), depCtl, t('여러 부서에 속할 수 있습니다. 주소의 부서 부분과 클라이언트의 부서 선택에 쓰입니다')),
        row(t('새 비밀번호'), f.password),
        row(t('기기 수 한도'), f.max_devices, t('비우면 정책 기본값. 1~9999')),
        row(t('저장 한도 (MB)'), f.quota_mb, t('이 계정이 보낸 첨부파일의 서버 보관 용량. 비우면 정책 기본값, 0은 무제한')),
        row(t('메모'), f.note)),
      sect(t('기기'), { sub: t('{n}대', { n: devs.length }), right: h('a', { class: 'lnk', href: '#/devices', onclick: () => dr.close() }, t('기기 관리')) }, devGrid),
    ], [
      h('button', { class: 'btn dan', onclick: del }, t('계정 삭제')), h('span', { class: 'sp' }),
      h('button', { class: 'btn pri', onclick: save }, t('저장')),
      h('button', { class: 'btn', onclick: () => dr.close() }, t('취소')),
    ]);
    async function save() {
      const p = {};
      const nOrNull = (el) => (el.value === '' ? null : Number(el.value));
      if (f.state.value !== a.state) p.state = f.state.value;
      if (f.username.value.trim() !== (a.username || '')) p.username = f.username.value.trim();
      if (f.name.value.trim() !== a.name) p.name = f.name.value.trim();
      if (f.password.value) p.password = f.password.value;
      if (nOrNull(f.max_devices) !== (a.max_devices ?? null)) p.max_devices = nOrNull(f.max_devices);
      if (nOrNull(f.quota_mb) !== (a.quota_mb ?? null)) p.quota_mb = nOrNull(f.quota_mb);
      if (f.note.value !== a.note) p.note = f.note.value;
      const want = depBoxes.filter((b) => b.checked).map((b) => b.value).sort();
      if (JSON.stringify(want) !== JSON.stringify([...a.departments].sort())) p.departments = want;
      if (!Object.keys(p).length) return dr.close();
      try { await api('PATCH', `/api/accounts/${a.id}`, p); toast(t('저장했습니다')); dr.close(); reload(); } catch (e) { fail(e); }
    }
    async function del() {
      if (!(await confirmBox(t('계정 삭제'), [t('{name} 계정을 삭제할까요?', { name: a.username || a.name || a.id }), h('div', { class: 'warnbox' }, t('등록된 기기 {d}대와 받지 못한 쪽지 {p}건이 함께 삭제됩니다. 되돌릴 수 없습니다.', { d: devs.length, p: a.pending }))], t('삭제'), true))) return;
      try { await api('DELETE', `/api/accounts/${a.id}`); toast(t('삭제했습니다')); dr.close(); reload(); } catch (e) { fail(e); }
    }
  }

  c.append(sec);
  await reload();
  const want = hashQ.get('id');
  if (want) {
    const a = rows.find((x) => x.id === want) || (await api('GET', `/api/accounts?q=${encodeURIComponent(want)}&per=10`)).items.find((x) => x.id === want);
    if (a) open(a);
  }
}

// ---------------------------------------------------------------- devices
async function pDevices(c) {
  const L = lister('/api/devices', { filter: 'all' });
  const seg = h('div', { class: 'seg' });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const pg = h('div');
  const sec = sect(t('기기'), { tasks: [[t('새로 고침'), () => L.reload()]] },
    h('div', { class: 'tb' }, seg, L.search(t('필터: 이름, 계정, 기기 ID, IP, 호스트')), h('span', { class: 'sp' }), pg), host, foot);
  async function reload() {
    const r = await L.fetch();
    const k = r.counts;
    seg.replaceChildren(...[['all', t('전체 {n}', { n: num(k.all) })], ['online', t('접속 중 {n}', { n: num(k.online) })], ['disabled', t('정지 {n}', { n: num(k.disabled) })], ['waiting', t('받을 쪽지 있음 {n}', { n: num(k.waiting) })]]
      .map(([f, x]) => h('button', { class: L.st.filter === f ? 'on' : '', onclick: () => { L.st.filter = f; L.st.page = 1; L.reload(); } }, x)));
    host.replaceChildren(grid('devices', [
      { k: 'online', t: '', sort: false, v: (x) => h('span', { class: 'dot' + (x.online ? ' ok' : ''), title: x.online ? t('접속 중') : t('오프라인') }) },
      { k: 'name', t: t('기기 이름'), v: (x) => x.profile.name || h('span', { class: 'mu' }, '—') },
      { k: 'account', t: t('계정@@one'), v: (x) => h('a', { href: `#/accounts?state=all&id=${x.account}` }, x.username || x.account_name || short(x.account)) },
      { k: 'account_state', t: t('계정 상태'), v: (x) => stateChip(x.account_state) },
      { k: 'status', t: t('상태 설정'), v: (x) => (x.profile.status ? statusChip(x.profile.status) : '') },
      { k: 'host', t: t('호스트'), v: (x) => x.profile.host || '' },
      { k: 'id', t: t('기기 ID'), cls: 'mono' },
      { k: 'app', t: t('앱'), cls: 'mono' },
      { k: 'last_ip', t: t('마지막 IP'), cls: 'mono' },
      { k: 'key', t: t('E2EE 키 지문'), cls: 'mono', sort: false, v: (x) => x.key || h('span', { class: 'mu' }, t('없음')) },
      { k: 'limits', t: t('수신 한도'), cls: 'num', sort: false, v: (x) => t('{n}개 · {s}', { n: x.profile.max_files || '∞', s: x.profile.max_file_size ? bytes(x.profile.max_file_size) : '∞' }) },
      { k: 'pending', t: t('받을 쪽지'), cls: 'num', v: (x) => (x.pending ? num(x.pending) : h('span', { class: 'mu' }, '0')) },
      { k: 'last_seen', t: t('마지막 접속'), v: (x) => ago(x.last_seen) },
      { k: 'created', t: t('등록'), v: (x) => h('span', { title: tsFull(x.created) }, ts(x.created)) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (x) => [
        x.online && h('button', { class: 'btn sm', onclick: () => act(x, 'kick') }, t('끊기')),
        h('button', { class: 'btn sm', onclick: () => act(x, x.disabled ? 'enable' : 'disable') }, x.disabled ? t('정지 해제') : t('정지')),
        h('button', { class: 'btn sm dan', onclick: () => act(x, 'delete') }, t('등록 해제')),
      ] },
    ], r.items, { order: L.order(), empty: t('기기가 없습니다') }));
    pg.replaceChildren(L.pager(r.total));
    sec.sub(t('전체 {a} | 접속 중 {o} | 조건에 맞는 {n}개', { a: num(k.all), o: num(k.online), n: num(r.total) }));
    foot.replaceChildren(h('span', {}, t('조건에 맞는 {n}개', { n: num(r.total) })));
  }
  L.reload = () => reload().catch(fail);
  async function act(d, what) {
    const name = d.profile.name || d.id;
    const T = {
      kick: [t('연결 끊기'), [t('{name} 기기의 연결을 끊을까요?', { name })], t('끊기')],
      disable: [t('기기 정지'), [t('{name} 기기를 정지할까요?', { name }), t('정지된 기기는 로그온할 수 없고, 연락처에서도 빠집니다.')], t('정지')],
      enable: [t('기기 정지 해제'), [t('{name} 기기의 정지를 풀까요?', { name })], t('정지 해제')],
      delete: [t('기기 등록 해제'), [t('{name} 기기의 등록을 해제할까요?', { name }), h('div', { class: 'warnbox' }, t('이 기기가 받지 못한 쪽지 {n}건이 삭제됩니다. 기기는 다음 로그온 때 새 기기로 다시 등록됩니다 (기기 수 한도 적용).', { n: d.pending }))], t('등록 해제')],
    }[what];
    if (!(await confirmBox(T[0], T[1], T[2], what !== 'enable'))) return;
    try {
      if (what === 'kick') await api('POST', `/api/devices/${encodeURIComponent(d.id)}/kick`);
      else if (what === 'delete') await api('DELETE', `/api/devices/${encodeURIComponent(d.id)}`);
      else await api('PATCH', `/api/devices/${encodeURIComponent(d.id)}`, { disabled: what === 'disable' });
      toast(t('처리했습니다'));
      reload();
    } catch (e) { fail(e); }
  }
  c.append(sec);
  await reload();
}

// ---------------------------------------------------------------- stored messages
async function pMessages(c, ph) {
  const L = lister('/api/messages');
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const pg = h('div');
  ph.append(h('span', { class: 'sub' }, t('평문 쪽지는 행을 눌러 내용과 첨부파일을 열람할 수 있습니다. 열람 기록은 감사 로그에 남습니다')));
  const sec = sect(t('보관 중인 쪽지'), { tasks: [[t('만료 처리 지금 실행'), sweep], '-', [t('새로 고침'), () => L.reload()]] },
    h('div', { class: 'tb' }, L.search(t('필터: 보낸 사람, 받는 사람, ID')), h('span', { class: 'sp' }), h('button', { class: 'btn', onclick: sweep }, t('만료 처리 지금 실행')), h('span', { class: 'tsep' }), pg), host, foot);
  async function reload() {
    const r = await L.fetch();
    host.replaceChildren(grid('messages', [
      { k: 'created', t: t('보관 시작'), v: (x) => h('span', { title: tsFull(x.created) }, ts(x.created)) },
      { k: 'from_name', t: t('보낸 사람'), v: (x) => h('a', { href: `#/accounts?state=all&id=${x.from}` }, x.from_name || short(x.from)) },
      { k: 'to', t: t('받는 사람'), cls: 'trunc', sort: false, v: (x) => h('span', { title: x.to || '' }, x.to || '—') },
      { k: 'pending', t: t('전달 대기'), cls: 'num', v: (x) => t('{a} / {b} 기기', { a: x.pending, b: x.deliveries }) },
      { k: 'files', t: t('첨부'), cls: 'num', v: (x) => (x.files ? (x.files_gone ? h('span', { class: 'mu' }, t('{n}개 (삭제됨)', { n: x.files })) : t('{n}개', { n: x.files })) : h('span', { class: 'mu' }, '—')) },
      { k: 'bytes', t: t('크기'), cls: 'num', v: (x) => (x.bytes ? bytes(x.bytes) : h('span', { class: 'mu' }, '—')) },
      { k: 'enc', t: t('본문'), v: (x) => (x.enc === 'none' ? chip('ne', t('평문')) : chip('ok', t('암호화'))) },
      { k: 'files_expires', t: t('첨부 만료'), v: (x) => (!x.files || x.files_gone ? h('span', { class: 'mu' }, '—') : until(x.files_expires)) },
      { k: 'text_expires', t: t('쪽지 만료'), v: (x) => until(x.text_expires) },
      { k: 'mid', t: 'ID', cls: 'mono' },
      { k: 'act', t: '', sort: false, cls: 'act', v: (x) => h('button', { class: 'btn sm dan', onclick: () => purge(x) }, t('삭제')) },
    ], r.items, { key: 'mid', order: L.order(), onRow: (x) => openMessage(x.mid, L.reload), empty: t('보관 중인 쪽지가 없습니다') }));
    pg.replaceChildren(L.pager(r.total));
    sec.sub(t('{n}건 | 첨부 {b}', { n: num(r.total), b: bytes(r.bytes) }));
    foot.replaceChildren(h('span', {}, t('조건에 맞는 {n}건 · 첨부 {b} · 전달 대기 {p}건 (기기 단위)', { n: num(r.total), b: bytes(r.bytes), p: num(r.pending) })));
  }
  L.reload = () => reload().catch(fail);
  async function purge(m) {
    if (!(await confirmBox(t('보관 쪽지 삭제'), [t('{name}님이 보낸 쪽지를 지금 삭제할까요?', { name: m.from_name }), t('아직 받지 못한 기기 {n}대는 이 쪽지를 받을 수 없게 되며, 만료 알림도 남지 않습니다.', { n: m.pending })], t('삭제'), true))) return;
    try { await api('DELETE', `/api/messages/${m.mid}`); toast(t('삭제했습니다')); reload(); } catch (e) { fail(e); }
  }
  async function sweep() {
    try {
      const r = await api('POST', '/api/sweep');
      toast(t('첨부 {f}건 · 쪽지 {m}건 만료 처리 (만료 알림 {n}건)', { f: r.files, m: r.messages, n: r.notices }));
      reload();
    } catch (e) { fail(e); }
  }
  c.append(sec);
  await reload();
}

// Shared pieces of the message viewers.
const msgBody = (text) => h('div', { class: 'msgbody' }, text || h('span', { class: 'mu' }, t('(본문 없음)')));
function groupBox(g) {
  if (!g || !g.name) return null;
  return props([
    [t('그룹'), g.name],
    [t('그룹 구성원'), (g.members || []).map((m) => m.name || m.id).join(', ') || '—'],
  ], 'one');
}
function fileTable(id, files, href, kept) {
  return grid(id, [
    { k: 'idx', t: '#', cls: 'num', v: (f) => f.idx + 1 },
    { k: 'name', t: t('파일 이름'), v: (f) => f.name || h('span', { class: 'mu' }, `file${f.idx + 1}`) },
    { k: 'size', t: t('크기'), cls: 'num', v: (f) => bytes(f.size) },
    { k: 'act', t: '', sort: false, cls: 'act', v: (f) => (kept(f)
      ? h('a', { class: 'btn sm', href: href(f), download: f.name || '' }, t('다운로드'))
      : h('span', { class: 'mu' }, t('보관 안 됨'))) },
  ], files, { key: 'idx', empty: t('첨부파일 없음') });
}
const recipientNames = (rs) => (rs || []).map((x) => x.username || x.name || short(x.account)).join(', ');

// A message still waiting on the relay. Opening a plaintext one is audited server-side.
async function openMessage(mid) {
  let m;
  try { m = await api('GET', `/api/messages/${mid}`); } catch (e) { return fail(e); }
  const plain = m.enc === 'none';
  const acked = m.deliveries.filter((d) => d.acked).length;
  const dr = drawer(t('보관 쪽지 · {name}', { name: m.from_name }), [
    props([
      [t('보낸 사람'), [h('a', { href: `#/accounts?state=all&id=${m.from}` }, m.from_name || short(m.from)), ' ', h('span', { class: 'mono mu' }, m.from_device)]],
      [t('보관 시작'), tsFull(m.created)],
      [t('쪽지 만료'), until(m.text_expires)],
      [t('첨부 만료'), m.files.length ? (m.files_gone ? t('삭제됨') : until(m.files_expires)) : '—'],
      [t('본문'), plain ? chip('ne', t('평문')) : chip('ok', t('종단간 암호화'))],
      [t('쪽지 ID'), h('span', { class: 'mono' }, m.mid)],
    ], 'one'),
    plain
      ? [sect(t('내용'), {}, h('div', { class: 'pad' }, msgBody(m.content.text))), groupBox(m.content.group)]
      : h('div', { class: 'warnbox' }, t('종단간 암호화된 쪽지입니다. 서버는 본문, 파일 이름, 첨부파일 내용을 볼 수 없습니다.')),
    sect(t('첨부파일'), { sub: t('{n}개', { n: m.files.length }) },
      fileTable('msg-files', m.files, (f) => tq(`/api/messages/${m.mid}/files/${f.idx}`), (f) => f.available)),
    sect(t('전달 상태'), { sub: t('{a}/{b} 기기 수신', { a: acked, b: m.deliveries.length }) },
      grid('msg-deliv', [
        { k: 'account_name', t: t('받는 사람'), v: (d) => h('a', { href: `#/accounts?state=all&id=${d.account}` }, d.account_name || short(d.account)) },
        { k: 'device_name', t: t('기기@@one'), v: (d) => [d.device_name || '—', ' ', h('span', { class: 'mono mu' }, d.device)] },
        { k: 'acked', t: t('수신'), v: (d) => (d.acked ? h('span', { title: tsFull(d.acked) }, chip('ok', t('받음')), ' ', ts(d.acked)) : chip('wa', t('대기'))) },
      ], m.deliveries, { key: 'device' })),
  ], [
    h('span', { class: 'mu' }, plain ? t('이 열람은 감사 로그에 기록되었습니다') : ''), h('span', { class: 'sp' }),
    h('button', { class: 'btn', onclick: () => dr.close() }, t('닫기')),
  ]);
}

// ---------------------------------------------------------------- message audit (archive)
async function pArchive(c, ph) {
  const { settings } = await api('GET', '/api/settings');
  const day = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const from = h('input', { type: 'date', value: day(new Date(Date.now() - 7 * 86400000)) });
  const to = h('input', { type: 'date', value: day(new Date()) });
  const q = h('input', { type: 'search', placeholder: t('본문, 보낸 사람, 받는 사람, 파일 이름') });
  const go = h('button', { class: 'btn pri', onclick: () => { page = 1; load(); } }, t('조회'));
  const pg = h('div');
  let page = 1;
  let per = 100;
  let query = null; // the filters of the last search, kept while paging
  const host = h('div', {}, h('div', { class: 'empty' }, t('조건을 정하고 조회하세요. 조회할 때마다 감사 로그에 기록됩니다.')));
  const foot = h('div', { class: 'foot' });
  let rows = [];
  q.addEventListener('keydown', (e) => { if (e.key === 'Enter') { page = 1; load(); } });
  ph.append(h('span', { class: 'sub' }, t('종단간 암호화가 아닌 쪽지의 사본 (전달 후에도 보관)')));
  async function exportCsv() {
    if (!query) return toast(t('먼저 조회하세요'), true);
    try {
      const all = (await api('GET', `/api/archive?${new URLSearchParams({ ...query, page: 1, per: 20000 })}`)).items;
      csv(`relay-messages-${day(new Date())}.csv`, ['time', 'from', 'from_account', 'to', 'text', 'files', 'bytes', 'id'],
        all.map((r) => [tsFull(r.created), r.from_name, r.from, recipientNames(r.recipients), r.preview + (r.length > r.preview.length ? '…' : ''), r.files.map((f) => f.name).join(' | '), r.bytes, r.mid]));
    } catch (e) { fail(e); }
  }
  const sec = sect(t('쪽지 감사'), { tasks: [[t('CSV 내보내기 (최대 2만 건)'), exportCsv]] },
    h('div', { class: 'tb' }, h('span', { class: 'tl' }, t('기간')), from, h('span', { class: 'mu' }, '~'), to, h('span', { class: 'filter' }, q), go, h('span', { class: 'sp' }), pg), host, foot);
  async function load() {
    go.disabled = true;
    try {
      const f = from.value ? new Date(`${from.value}T00:00:00`).getTime() : '';
      const tt = to.value ? new Date(`${to.value}T00:00:00`).getTime() + 86400000 : '';
      if (page === 1 || !query) query = { q: q.value.trim(), ...(f ? { from: f } : {}), ...(tt ? { to: tt } : {}) };
      const r = await api('GET', `/api/archive?${new URLSearchParams({ ...query, page, per })}`);
      rows = r.items;
      host.replaceChildren(grid('archive', [
        { k: 'created', t: t('보낸 시각'), v: (x) => h('span', { title: tsFull(x.created) }, ts(x.created)) },
        { k: 'from_name', t: t('보낸 사람'), v: (x) => x.from_name || short(x.from) },
        { k: 'to', t: t('받는 사람'), cls: 'trunc', s: (x) => recipientNames(x.recipients), v: (x) => h('span', { title: recipientNames(x.recipients) }, recipientNames(x.recipients)) },
        { k: 'preview', t: t('내용'), cls: 'trunc wide', v: (x) => h('span', { title: x.preview }, x.preview.replace(/\s+/g, ' ') || h('span', { class: 'mu' }, t('(본문 없음)'))) },
        { k: 'group', t: t('그룹'), s: (x) => x.group?.name, v: (x) => x.group?.name || '' },
        { k: 'files', t: t('첨부'), cls: 'num', s: (x) => x.files.length, v: (x) => (x.files.length ? (x.files_kept ? t('{n}개', { n: x.files.length }) : t('{n}개 (미보관)', { n: x.files.length })) : '') },
        { k: 'bytes', t: t('크기'), cls: 'num', v: (x) => (x.bytes ? bytes(x.bytes) : '') },
      ], rows, { key: 'mid', onRow: (x) => openArchived(x.mid, load), empty: t('조건에 맞는 쪽지가 없습니다') }));
      pg.replaceChildren(pagerBox(page, per, r.total, (n) => { page = n; load(); }, (n) => { per = n; page = 1; load(); }));
      sec.sub(t('조건에 맞는 {n}건 | 전체 보관 {a}건', { n: num(r.total), a: num(r.all) }));
      foot.replaceChildren(h('span', {}, t('조건에 맞는 {n}건 · 전체 보관 {a}건', { n: num(r.total), a: num(r.all) })));
    } catch (e) { fail(e); } finally { go.disabled = false; }
  }
  c.append(...[
    !settings.archive && h('div', { class: 'warnbox' }, t('감사 보관이 꺼져 있어 새 쪽지는 보관되지 않습니다. '), h('a', { href: '#/policy' }, t('정책 > 쪽지 감사')), t('에서 켤 수 있습니다. 지금 서버에 맡겨진 쪽지는 "보관함"에서 열람할 수 있습니다.')),
    settings.archive && settings.policy.encryption === 'e2ee' && h('div', { class: 'warnbox' }, t('현재 암호화 단계가 종단간 암호화라서 새 쪽지는 서버가 내용을 볼 수 없어 보관되지 않습니다.')),
    sec].filter(Boolean));
}

async function openArchived(mid, reload) {
  let m;
  try { m = await api('GET', `/api/archive/${mid}`); } catch (e) { return fail(e); }
  const dr = drawer(t('쪽지 감사 · {name}', { name: m.from_name }), [
    props([
      [t('보낸 사람'), [h('a', { href: `#/accounts?state=all&id=${m.from}` }, m.from_name || short(m.from)), ' ', h('span', { class: 'mono mu' }, m.from_device)]],
      [t('받는 사람'), (m.recipients || []).map((x, i) => [i ? ', ' : '', h('a', { href: `#/accounts?state=all&id=${x.account}` }, x.username || x.name || short(x.account)), h('span', { class: 'mu' }, t(' ({n}대)', { n: x.devices }))])],
      [t('보낸 시각'), tsFull(m.created)],
      [t('쪽지 ID'), h('span', { class: 'mono' }, m.mid)],
    ], 'one'),
    sect(t('내용'), {}, h('div', { class: 'pad' }, msgBody(m.text))),
    groupBox(m.group),
    sect(t('첨부파일'), { sub: [t('{n}개', { n: m.files.length }), m.files.length && !m.files_kept ? t(' · 첨부파일은 보관하지 않도록 설정되어 있었습니다') : ''] },
      fileTable('arc-files', m.files, (f) => tq(`/api/archive/${m.mid}/files/${f.idx}`), () => m.files_kept)),
  ], [
    h('button', { class: 'btn dan', onclick: del }, t('감사 사본 삭제')), h('span', { class: 'mu' }, t('이 열람은 감사 로그에 기록되었습니다')), h('span', { class: 'sp' }),
    h('button', { class: 'btn', onclick: () => dr.close() }, t('닫기')),
  ]);
  async function del() {
    if (!(await confirmBox(t('감사 사본 삭제'), [t('이 쪽지의 감사 사본(본문과 보관된 첨부파일)을 삭제할까요? 삭제 기록은 감사 로그에 남습니다.')], t('삭제'), true))) return;
    try { await api('DELETE', `/api/archive/${m.mid}`); toast(t('삭제했습니다')); dr.close(); reload(); } catch (e) { fail(e); }
  }
}

// ---------------------------------------------------------------- audit
const ACTIONS = {
  admin_setup: '최초 관리자 생성', admin_login: '관리자 로그인', admin_login_failed: '관리자 로그인 실패', admin_create: '관리자 추가@@ev',
  admin_delete: '관리자 삭제@@ev', admin_password: '관리자 비밀번호 변경', settings: '정책 변경', account_create: '계정 생성',
  account_update: '계정 수정', account_delete: '계정 삭제@@ev', account_approve: '계정 승인@@ev', account_enable: '계정 정지 해제@@ev',
  account_disable: '계정 정지@@ev', register: '가입', device_add: '기기 추가', device_move: '기기 계정 이동', device_disable: '기기 정지@@ev',
  device_enable: '기기 정지 해제@@ev', device_delete: '기기 등록 해제@@ev', device_kick: '연결 끊기@@ev', message_purge: '보관 쪽지 삭제@@ev', sweep: '만료 처리 실행', login_locked: '로그인 잠금',
  message_view: '보관 쪽지 열람', message_file: '보관 첨부 다운로드', archive_list: '쪽지 감사 조회', archive_view: '쪽지 감사 열람',
  archive_file: '감사 첨부 다운로드', archive_delete: '감사 사본 삭제@@ev',
  user_login: '포털 로그인', user_login_failed: '포털 로그인 실패', user_password: '포털 비밀번호 변경', user_name: '포털 이름 변경',
  user_device_kick: '포털 기기 연결 끊기', user_device_delete: '포털 기기 등록 해제',
  server_settings: '서버 설정 변경', tenancy_create: 'Tenancy 생성', tenancy_update: 'Tenancy 수정', tenancy_delete: 'Tenancy 삭제@@ev',
  tenancy_purge: 'Tenancy 사용자 일괄 삭제', account_purge: '사용자 일괄 삭제@@ev', account_move: '계정 Tenancy 이동',
  account_moved_in: '다른 Tenancy에서 옮겨 옴', account_moved_out: '다른 Tenancy로 옮겨 감',
  department_create: '부서 생성', department_update: '부서 수정@@ev', department_delete: '부서 삭제@@ev', department_members: '부서 구성원 변경',
  api_key_create: 'API 키 발급@@ev', api_key_revoke: 'API 키 폐기@@ev', relay_receive: '릴레이 수신 설정',
};
async function pAudit(c) {
  const L = lister('/api/audit');
  const act = h('select', { title: t('작업 종류'), onchange: () => { L.st.filter = act.value; L.st.page = 1; L.reload(); } });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const pg = h('div');
  async function exportCsv() {
    try {
      const qs = new URLSearchParams({ page: 1, per: 20000, q: L.st.q, filter: L.st.filter });
      const all = (await api('GET', `/api/audit?${qs}`)).items;
      csv(`relay-audit-${ts(Date.now()).replace(/[: ]/g, '')}.csv`, ['time', 'actor', 'action', 'detail', 'account'], all.map((r) => [tsFull(r.ts), r.actor, r.action, r.detail, r.account || '']));
    } catch (e) { fail(e); }
  }
  const sec = sect(TEN ? t('감사 로그') : t('서버 감사 로그'), { tasks: [[t('CSV 내보내기 (최근 2만 건)'), exportCsv], '-', [t('새로 고침'), () => L.reload()]] },
    h('div', { class: 'tb' }, act, L.search(t('필터: 주체, 내용')), h('span', { class: 'sp' }), pg), host, foot);
  async function reload() {
    const r = await L.fetch();
    act.replaceChildren(h('option', { value: '' }, t('모든 작업')), ...r.actions.map((a) => h('option', { value: a, selected: a === L.st.filter }, tr(ACTIONS, a))));
    host.replaceChildren(grid('audit', [
      { k: 'ts', t: t('시각'), sort: false, v: (x) => h('span', { class: 'mono' }, tsFull(x.ts)) },
      { k: 'actor', t: t('주체'), cls: 'mono', sort: false },
      { k: 'action', t: t('작업@@event'), sort: false, v: (x) => h('span', { title: x.action }, tr(ACTIONS, x.action)) },
      { k: 'detail', t: t('내용'), cls: 'wrap', sort: false },
    ], r.items, { key: 'ts', empty: t('기록이 없습니다') }));
    pg.replaceChildren(L.pager(r.total));
    sec.sub(t('조건에 맞는 {n}건 | 최신순', { n: num(r.total) }));
    foot.replaceChildren(h('span', {}, t('조건에 맞는 {n}건 · 최신순', { n: num(r.total) })));
  }
  L.reload = () => reload().catch(fail);
  c.append(sec);
  await reload();
}

// ---------------------------------------------------------------- policy
// A tenancy's own policy (its admins). Texts are Korean source strings.
const POLICY = [
  ['연락처', [
    { p: 'policy.contacts', t: '연락처 공개 범위', type: 'select', opts: [['all', '전체 공개 — 모든 사용자가 서로 보임'], ['lookup', '검색 — 주고받았거나 직접 찾은 사용자만']], help: '사용자가 많은 공개 Tenancy라면 "검색"을 권장합니다. 다른 Tenancy의 사용자는 어느 쪽이든 보이지 않습니다' },
    { p: 'show_host', t: '컴퓨터 이름 공개', type: 'bool', label: '연락처에 기기의 컴퓨터(호스트) 이름 포함' },
  ]],
  ['가입 및 로그온', [
    { p: 'policy.auth', t: '로그온 방식', type: 'select', opts: [['password', '사용자 이름 + 비밀번호'], ['none', '없음 — 기기 ID와 기기 비밀값만']], warn: '바꾸면 접속 중인 모든 기기의 연결이 끊기고, 다시 로그온해야 합니다', help: '"없음"이면 기기 하나가 계정 하나가 됩니다' },
    { p: 'policy.registration', t: '외부인 가입', type: 'select', opts: [['closed', '닫힘 — 관리자가 만든 계정만 (기본)'], ['approval', '승인제 — 신청을 받고 관리자가 승인'], ['open', '열림 — 누구나 바로 가입 (공개 서버용)']] },
    { p: 'policy.max_devices', t: '계정당 기기 수', type: 'num', min: 1, max: 9999, unit: '대', help: '계정마다 따로 정할 수 있습니다. 최대 9999대. API 키 기기는 세지 않습니다' },
    { p: 'min_password_len', t: '최소 비밀번호 길이', type: 'num', min: 4, max: 128, unit: '자', help: '관리자 계정은 8자 이상' },
    { p: 'login_max_failures', t: '로그인 실패 허용', type: 'num', min: 1, max: 1000, unit: '회', help: '사용자 이름별·기기별·API 키별로 셉니다' },
    { p: 'login_lockout_minutes', t: '잠금 시간', type: 'num', min: 1, max: 10080, unit: '분' },
  ]],
  ['경로', [
    { p: 'policy.force_relay', t: '릴레이 경유 강제', type: 'bool', label: '이 Tenancy 연락처에 있는 사람에게는 LAN에서 보여도 릴레이로 보냄' },
    { p: 'policy.block_direct_outsiders', t: '외부 LAN 직접 전송', type: 'bool', label: '이 Tenancy를 쓰지 않는 LAN 사용자에게 직접 보내는 것도 금지', dep: 'policy.force_relay', help: '경유 강제일 때만 적용됩니다. 서버는 LAN 전송을 볼 수 없으므로 클라이언트가 지킵니다' },
  ]],
  ['암호화', [
    { p: 'policy.encryption', t: '암호화 단계', type: 'select', opts: [['plain', '평문 — 암호화 없음'], ['tls', '서버 통신 암호화 — TLS 필수, 서버에는 평문 저장'], ['e2ee', '종단간 암호화 — TLS 필수, 서버는 내용을 볼 수 없음']], warn: '평문이 아닌 단계로 바꾸면 평문으로 접속한 기기의 연결이 끊깁니다. 종단간 암호화에서는 API 키를 쓸 수 없습니다', help: 'LAN 직접 전송에는 적용되지 않습니다' },
  ]],
  ['전송 한도 (릴레이를 거칠 때만)', [
    { p: 'policy.relay_max_files', t: '첨부파일 수', type: 'num', min: 0, max: 100000, unit: '개', help: '0이면 첨부파일을 중계하지 않습니다' },
    { p: 'policy.relay_max_file_size', t: '파일당 최대 크기', type: 'mb', unit: 'MB', help: '0이면 첨부파일을 중계하지 않습니다. E2EE에서는 암호문 크기 기준. 서버 총괄 관리자가 정한 상한을 넘을 수 없습니다' },
  ]],
  ['보관', [
    { p: 'policy.retention_hours.text', t: '쪽지 보관 시간', type: 'num', min: 1, max: 87600, unit: '시간', help: '지나면 삭제하고, 받지 못한 기기에는 보낸 사람별 만료 개수만 알립니다. 계정의 모든 기기가 받아야 전달 완료로 봅니다' },
    { p: 'policy.retention_hours.files', t: '첨부파일 보관 시간', type: 'num', min: 1, max: 87600, unit: '시간', help: '쪽지 보관 시간보다 길게 정해도 쪽지 보관 시간으로 맞춰집니다' },
    { p: 'account_quota_mb', t: '계정별 저장 한도', type: 'num', min: 0, unit: 'MB', help: '보낸 계정 기준. 0이면 무제한. 계정마다 따로 정할 수 있습니다. Tenancy 전체 한도는 서버 총괄 관리자가 정합니다' },
  ]],
  ['다른 Tenancy와 주고받기', [
    { p: 'exchange.send_local', t: '같은 서버로 보내기', type: 'bool', label: '이 서버의 다른 Tenancy로 쪽지와 첨부파일을 보낼 수 있음' },
    { p: 'exchange.receive_local', t: '같은 서버에서 받기', type: 'bool', label: '이 서버의 다른 Tenancy에서 보낸 쪽지와 첨부파일을 받음' },
    { p: 'exchange.send_remote', t: '외부 서버로 보내기', type: 'bool', label: '다른 서버의 Tenancy로 보낼 수 있음', help: '서버 사이 전송은 아직 지원하지 않습니다. 정책만 미리 정해 둘 수 있습니다' },
    { p: 'exchange.receive_remote', t: '외부 서버에서 받기', type: 'bool', label: '다른 서버의 Tenancy에서 보낸 쪽지를 받음' },
    { p: 'exchange.allow', t: '허용 목록', type: 'list', placeholder: 'acme\nsales:relay.partner.com\n*:relay.partner.com', help: '한 줄에 하나씩 "tenancy:서버". 서버를 빼면 이 서버, Tenancy 자리에 *를 쓰면 그 서버 전체. 비어 있지 않으면 목록에 있는 곳하고만 주고받습니다' },
    { p: 'exchange.deny', t: '차단 목록', type: 'list', placeholder: 'rival\n*:spam.example', help: '같은 형식. 여기 있으면 허용 목록과 관계없이 막습니다' },
  ]],
  ['API 키', [
    { p: 'api_keys', t: 'API 키 허용', type: 'bool', label: '사용자가 스크립트용 API 키(Bearer 토큰)를 발급해 쪽지를 보내고 받을 수 있음', help: '사용자 포털과 "API 키" 화면에서 발급합니다. 종단간 암호화 단계에서는 켤 수 없습니다. 끄면 발급된 키도 모두 막힙니다' },
  ]],
  ['쪽지 감사', [
    { p: 'archive', t: '감사 보관', type: 'bool', label: '종단간 암호화가 아닌 쪽지의 사본을 전달 후에도 보관', warn: '사용자에게는 알리지 않습니다. 조직의 정보 보호 방침에 따라 쓰세요', help: '켠 뒤부터 릴레이를 거치는 쪽지가 보관됩니다. 다른 Tenancy와 주고받은 쪽지도 포함됩니다. LAN 직접 전송은 서버를 거치지 않아 보관되지 않습니다' },
    { p: 'archive_files', t: '첨부파일 보관', type: 'bool', label: '감사 사본에 첨부파일도 함께 보관', dep: 'archive', help: '디스크를 많이 쓸 수 있습니다. 서버 총괄 관리자가 막아 둘 수 있습니다. 끄면 파일 이름과 크기만 남습니다' },
    { p: 'archive_days', t: '감사 보관 기간', type: 'num', min: 1, max: 3650, unit: '일', help: '지나면 사본과 첨부파일을 삭제합니다' },
  ]],
  ['기록', [
    { p: 'audit_days', t: '감사 로그 보관', type: 'num', min: 1, max: 3650, unit: '일' },
  ]],
];

// How the server itself runs (server admins).
const SERVER_SETTINGS = [
  ['서버', [
    { p: 'server_name', t: '서버 이름', type: 'text', help: '클라이언트와 이 관리 화면에 표시됩니다' },
    { p: 'domain', t: '서버 주소 (주소 체계)', type: 'text', placeholder: 'relay.example.com', help: '주소 "사용자@tenancy:서버"의 서버 부분. 비우면 사용자 접속 주소의 호스트, 그것도 없으면 인증서 이름을 씁니다' },
    { p: 'storage_quota_mb', t: '서버 전체 저장 한도', type: 'num', min: 0, unit: 'MB', help: '모든 Tenancy의 첨부파일 합계. 0이면 무제한. Tenancy별 한도는 Tenancy 화면에서 정합니다' },
  ]],
  ['Tenancy 사이의 교환', [
    { p: 'exchange.cross_tenancy', t: '서버 내 Tenancy 간', type: 'bool', label: '이 서버의 Tenancy끼리 쪽지와 첨부파일을 주고받을 수 있음', help: '양쪽 Tenancy도 각자 정책에서 허용해야 합니다. Tenancy별 예외는 Tenancy 화면에서 정합니다' },
    { p: 'exchange.send_remote', t: '외부 서버로 보내기', type: 'bool', label: '다른 서버로 보내는 것을 허용', help: '서버 사이 전송은 아직 지원하지 않습니다. 정책만 미리 정해 둘 수 있습니다' },
    { p: 'exchange.receive_remote', t: '외부 서버에서 받기', type: 'bool', label: '다른 서버에서 오는 것을 허용' },
  ]],
  ['외부 공개 (HTTPS 프록시 · WebSocket)', [
    { p: 'public_url', t: '사용자 접속 주소', type: 'text', help: '사용자에게 안내할 주소. 예: https://relay.example.com (Cloudflare Tunnel 등) 또는 relay.example.com:45443 (TCP 직접)' },
    { p: 'user_portal', t: '사용자 웹 포털', type: 'bool', label: '릴레이 주소를 브라우저로 열면 사용자가 자기 계정을 관리하는 화면을 보여 줌', help: '내 정보, 비밀번호 변경, 기기 관리, API 키, 내 활동 기록. 비밀번호 계정만 로그인할 수 있고, 암호화된 연결에서만 로그인됩니다. 스크립트용 API(/api/v1)는 꺼도 동작합니다' },
    { p: 'websocket', t: 'WebSocket 연결', type: 'bool', label: '릴레이 포트에서 WebSocket(HTTP 업그레이드)도 받음', help: 'Cloudflare Tunnel처럼 HTTP/HTTPS만 통과시키는 프록시를 거칠 때 필요합니다. 터널 서비스 주소는 http://localhost:45443' },
    { p: 'proxy_tls', t: '프록시가 TLS 처리', type: 'bool', label: '프록시가 HTTPS로 받았다고 표시한 WebSocket 연결을 암호화된 연결로 인정', dep: 'websocket', warn: '앞단 프록시가 HTTPS를 처리할 때만 켜세요. 켜면 프록시가 보낸 접속자 IP(CF-Connecting-IP / X-Forwarded-For)를 기록합니다', help: 'X-Forwarded-Proto: https 또는 CF-Visitor 헤더로 판단합니다' },
    { p: 'ws_chunk_kb', t: '스트리밍 단위', type: 'num', min: 16, max: 4096, unit: 'KB', dep: 'websocket', help: '첨부파일을 이 크기의 WebSocket 메시지로 나눠 보냅니다. HTTP 요청 하나로 올리지 않으므로 Cloudflare의 100MB 업로드 제한에 걸리지 않습니다. 기본 256KB' },
  ]],
  ['연결 및 관리', [
    { p: 'idle_timeout_secs', t: '무응답 연결 끊기', type: 'num', min: 45, max: 3600, unit: '초', help: '클라이언트는 30초마다 ping을 보냅니다' },
    { p: 'admin_session_hours', t: '관리자 세션 유지', type: 'num', min: 1, max: 720, unit: '시간', help: '마지막 사용 시각부터. 모든 관리자에게 적용됩니다' },
    { p: 'login_max_failures', t: '관리자 로그인 실패 허용', type: 'num', min: 1, max: 1000, unit: '회', help: '관리자 화면 로그인에만 적용됩니다. 사용자 로그인은 Tenancy 정책을 따릅니다' },
    { p: 'login_lockout_minutes', t: '관리자 잠금 시간', type: 'num', min: 1, max: 10080, unit: '분' },
    { p: 'audit_days', t: '서버 감사 로그 보관', type: 'num', min: 1, max: 3650, unit: '일', help: 'Tenancy 생성·삭제, 서버 설정, 총괄 관리자 기록' },
  ]],
];
const getP = (o, p) => p.split('.').reduce((a, k) => (a == null ? a : a[k]), o);
const setP = (o, p, v) => { const ks = p.split('.'); const last = ks.pop(); ks.reduce((a, k) => a[k], o)[last] = v; };
const MB = 1024 * 1024;

function pPolicy(c, ph) {
  return settingsPage(c, ph, {
    defs: POLICY,
    url: '/api/settings',
    sub: t('이 Tenancy의 정책입니다. 저장하면 접속 중인 클라이언트에 바로 알립니다'),
    before: (r) => sect(t('자원 한도'), { sub: t('서버 총괄 관리자가 정합니다') }, props([
      [t('저장 공간'), r.limits.storage_quota_mb ? `${num(r.limits.storage_quota_mb)} MB` : t('무제한')],
      [t('파일 크기 상한'), r.limits.max_file_size_mb ? `${num(r.limits.max_file_size_mb)} MB` : t('없음')],
      [t('본문 상한'), `${num(r.limits.max_body_kb)} KB`],
      [t('사용자 수'), r.limits.max_accounts ? t('{n}명', { n: num(r.limits.max_accounts) }) : t('무제한')],
      [t('감사 첨부 보관'), r.limits.archive_files ? t('허용') : t('불가')],
    ])),
    warns: (w) => [
      w.policy.retention_hours.files > w.policy.retention_hours.text && t('첨부파일 보관 시간이 쪽지 보관 시간보다 깁니다. 저장하면 쪽지 보관 시간으로 맞춰집니다.'),
      w.policy.registration === 'open' && t('외부인 가입이 "열림"입니다. 이 Tenancy에 접속할 수 있는 누구나 계정을 만들 수 있습니다.'),
      w.policy.encryption === 'plain' && t('암호화가 "평문"입니다. 신뢰할 수 있는 내부망에서만 쓰세요.'),
      w.archive && w.policy.encryption === 'e2ee' && t('감사 보관이 켜져 있지만 암호화 단계가 종단간 암호화라서 쪽지 내용은 보관되지 않습니다.'),
      w.api_keys && w.policy.encryption === 'e2ee' && t('종단간 암호화 단계에서는 API 키를 쓸 수 없어 저장하면 꺼집니다.'),
      w.policy.auth === 'none' && w.policy.registration === 'open' && t('로그온 방식 "없음" + 가입 "열림": 서버에 닿는 모든 기기가 바로 사용자가 됩니다.'),
    ],
  });
}

function pServerSettings(c, ph) {
  return settingsPage(c, ph, {
    defs: SERVER_SETTINGS,
    url: '/api/server/settings',
    sub: t('서버 전체에 적용됩니다. 사용자 정책은 Tenancy마다 따로 정합니다'),
    warns: (w) => [
      w.exchange.cross_tenancy && t('서버 내 Tenancy 간 전송이 켜져 있습니다. 실제로 주고받으려면 양쪽 Tenancy도 정책에서 허용해야 합니다.'),
    ],
  });
}

async function settingsPage(c, ph, { defs, url, sub, before, warns }) {
  const first = await api('GET', url);
  let orig = first.settings;
  let w = structuredClone(orig);
  const rows = [];
  const dirtyInfo = h('span', { class: 'mu' });
  const saveBtn = h('button', { class: 'btn pri', disabled: true, onclick: save }, t('저장'));
  const revertBtn = h('button', { class: 'btn', disabled: true, onclick: () => { w = structuredClone(orig); build(); } }, t('되돌리기'));
  ph.append(h('span', { class: 'sub' }, sub));
  const form = h('div', { class: 'form' });
  const note = h('div', { class: 'stack' });

  const label = (it, v) => {
    if (it.type === 'bool') return v ? t('켬') : t('끔');
    if (it.type === 'select') return t((it.opts.find((o) => o[0] === v) || [, v])[1]).split(' — ')[0];
    if (it.type === 'mb') return `${+(v / MB).toFixed(2)} MB`;
    if (it.type === 'list') return v.length ? v.join(', ') : t('(없음)');
    return `${v}${it.unit ? ' ' + t(it.unit) : ''}`;
  };
  function changes() {
    return defs.flatMap(([, items]) => items).filter((it) => JSON.stringify(getP(w, it.p)) !== JSON.stringify(getP(orig, it.p)));
  }
  function refresh() {
    const ch = changes();
    rows.forEach(([it, row, ctl]) => {
      row.classList.toggle('dirty', ch.includes(it));
      if (it.dep) ctl.disabled = !getP(w, it.dep);
    });
    saveBtn.disabled = revertBtn.disabled = !ch.length;
    dirtyInfo.textContent = ch.length ? t('변경 {n}건', { n: ch.length }) : t('변경 사항 없음');
    note.replaceChildren(...warns(w).filter(Boolean).map((x) => h('div', { class: 'warnbox' }, x)));
  }
  function control(it) {
    const v = getP(w, it.p);
    const set = (x) => { setP(w, it.p, x); refresh(); };
    if (it.type === 'bool') {
      const cb = h('input', { type: 'checkbox', checked: !!v, onchange: () => set(cb.checked) });
      return [cb, h('label', { class: 'line' }, cb, it.label ? t(it.label) : '')];
    }
    if (it.type === 'select') {
      const s = h('select', { onchange: () => set(s.value) }, it.opts.map(([k, x]) => h('option', { value: k, selected: k === v }, t(x))));
      return [s, s];
    }
    if (it.type === 'text') {
      const i = h('input', { type: 'text', value: v, placeholder: it.placeholder || '', oninput: () => set(i.value) });
      return [i, i];
    }
    if (it.type === 'list') {
      const ta = h('textarea', { rows: 4, placeholder: it.placeholder || '', value: v.join('\n'), oninput: () => set(ta.value.split('\n').map((x) => x.trim()).filter(Boolean)) });
      return [ta, ta];
    }
    if (it.type === 'mb') {
      const i = h('input', { type: 'number', min: 0, step: 'any', value: +(v / MB).toFixed(2), oninput: () => set(Math.max(0, Math.round((Number(i.value) || 0) * MB))) });
      return [i, h('span', { class: 'line' }, i, h('span', { class: 'unit' }, it.unit))];
    }
    const i = h('input', { type: 'number', min: it.min, max: it.max, value: v, oninput: () => set(Math.max(it.min ?? 0, Math.min(it.max ?? Number.MAX_SAFE_INTEGER, Math.floor(Number(i.value) || 0)))) });
    return [i, h('span', { class: 'line' }, i, h('span', { class: 'unit' }, it.unit ? t(it.unit) : ''), it.max && h('span', { class: 'mu' }, `(${num(it.min)}~${num(it.max)})`))];
  }
  function build() {
    rows.length = 0;
    form.replaceChildren(...defs.map(([title, items]) => h('div', { class: 'fs' }, h('div', { class: 'hd' }, t(title)), items.map((it) => {
      const [ctl, node] = control(it);
      const row = h('div', { class: 'row' }, h('label', {}, t(it.t)), h('div', { class: 'ctl' }, node,
        it.help && h('div', { class: 'help' }, t(it.help)),
        it.warn && h('div', { class: 'help err' }, '⚠ ' + t(it.warn))));
      rows.push([it, row, ctl]);
      return row;
    }))));
    refresh();
  }
  async function save() {
    const ch = changes();
    const lines = ch.map((it) => h('li', {}, h('b', {}, t(it.t)), `: ${label(it, getP(orig, it.p))} → ${label(it, getP(w, it.p))}`));
    const warn = ch.filter((it) => it.warn).map((it) => h('div', { class: 'warnbox' }, t(it.warn)));
    if (!(await confirmBox(t('설정 저장'), [h('ul', {}, lines), ...warn], t('저장')))) return;
    try {
      const r = await api('PUT', url, w);
      orig = r.settings;
      w = structuredClone(orig);
      build();
      toast(t('저장했습니다 ({n}건)', { n: r.changes.length }));
    } catch (e) { fail(e); }
  }
  build();
  c.append(...[before && before(first), note, form].filter(Boolean));
  c.parentNode.append(h('div', { class: 'savebar' }, dirtyInfo, h('span', { class: 'sp' }), saveBtn, revertBtn));
}

// ---------------------------------------------------------------- admins
async function pAdmins(c) {
  const host = h('div');
  const sec = sect(TEN ? t('Tenancy 관리자') : t('서버 총괄 관리자'), { sub: TEN || '', tasks: [[t('관리자 추가…'), add]] },
    h('div', { class: 'tb' }, h('button', { class: 'btn pri', onclick: add }, t('관리자 추가'))), host);
  async function load() {
    const { admins } = await api('GET', '/api/admins');
    host.replaceChildren(grid('admins', [
      { k: 'username', t: t('이름'), v: (r) => [r.username, r.me && h('span', {}, ' ', chip('ac', t('나')))] },
      { k: 'created', t: t('생성'), v: (r) => ts(r.created) },
      { k: 'last_login', t: t('마지막 로그인'), v: (r) => ago(r.last_login) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (r) => !r.me && h('button', { class: 'btn sm dan', onclick: () => del(r) }, t('삭제')) },
    ], admins));
    sec.sub(TEN ? `${TEN} | ` : '', t('{n}명', { n: admins.length }));
  }
  async function del(a) {
    if (!(await confirmBox(t('관리자 삭제'), [t('{name} 관리자를 삭제할까요? 로그인 중이면 바로 로그아웃됩니다.', { name: a.username })], t('삭제'), true))) return;
    try { await api('DELETE', `/api/admins/${a.id}`); toast(t('삭제했습니다')); load(); } catch (e) { fail(e); }
  }
  async function add() {
    const u = h('input', { type: 'text', autocomplete: 'off' });
    const p = h('input', { type: 'password', autocomplete: 'new-password' });
    if (await modal(t('관리자 추가'), [field(t('이름'), u), field(t('비밀번호 (8자 이상)'), p)], { ok: t('추가'), onOk: () => api('POST', '/api/admins', { username: u.value.trim(), password: p.value }) })) {
      toast(t('추가했습니다'));
      load();
    }
  }
  const old = h('input', { type: 'password', autocomplete: 'current-password' });
  const n1 = h('input', { type: 'password', autocomplete: 'new-password' });
  const n2 = h('input', { type: 'password', autocomplete: 'new-password' });
  async function change() {
    if (n1.value !== n2.value) return fail(t('새 비밀번호가 서로 다릅니다'));
    try { await api('POST', '/api/password', { old: old.value, new: n1.value }); toast(t('비밀번호를 바꿨습니다')); old.value = n1.value = n2.value = ''; } catch (e) { fail(e); }
  }
  const row = (label, ctl) => h('div', { class: 'row' }, h('label', {}, label), h('div', { class: 'ctl' }, ctl));
  c.append(
    h('div', { class: 'cols' },
      sec,
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('내 비밀번호 변경'), h('span', { class: 'mu' }, ME)),
        row(t('현재 비밀번호'), old), row(t('새 비밀번호'), n1), row(t('새 비밀번호 확인'), n2),
        h('div', { class: 'row' }, h('span'), h('div', { class: 'ctl' }, h('div', { class: 'line' }, h('button', { class: 'btn pri', onclick: change }, t('변경'))))))),
    h('div', { class: 'note mu' }, t('관리 화면은 기본적으로 서버 자신(127.0.0.1)에만 열려 있습니다. 원격에서 쓰려면 HTTPS 리버스 프록시나 SSH 터널을 거치세요.')));
  await load();
}

// ---------------------------------------------------------------- server (server admins)
const EX = { true: '허용', false: '금지' };
async function pServer(c, ph) {
  ph.append(h('span', { class: 'sub' }, t('서버와 모든 Tenancy의 상태')), h('span', { class: 'sp' }), refreshNote(10));
  const tiles = sect(t('Tenancy 및 서버'));
  const below = h('div', { class: 'stack' });
  c.append(tiles, below);
  let tileBox = null;
  async function draw() {
    const o = await api('GET', '/api/server');
    const s = o.server;
    const sum = (k) => o.tenancies.reduce((a, x) => a + (x[k] || 0), 0);
    const quota = s.storage_quota_mb ? o.stored_bytes / (s.storage_quota_mb * MB) : 0;
    tiles.sub(t('Tenancy {n} | 계정 {a} | 마지막 갱신 {t}', { n: num(o.tenancies.length), a: num(sum('accounts')), t: clock() }));
    const box = h('div', { class: 'tiles' },
      tile('Tenancy', num(o.tenancies.length), [
        [t('Unmigrated 포함'), num(o.tenancies.filter((x) => x.unmigrated).length)],
        [t('승인 대기가 있는 Tenancy'), num(o.tenancies.filter((x) => x.pending).length), o.tenancies.some((x) => x.pending) ? 'wa' : null],
        [t('옮겨 간 기기'), num(o.moved_devices)],
      ], '#/tenancies'),
      tile(t('접속'), num(sum('online')), [
        [t('접속 중인 기기'), num(sum('online'))],
        [t('평문 Tenancy'), num(o.tenancies.filter((x) => x.encryption === 'plain').length), o.tenancies.some((x) => x.encryption === 'plain') ? 'wa' : null],
      ]),
      tile(t('계정'), num(sum('accounts')), [
        [t('승인 대기'), num(sum('pending')), sum('pending') ? 'wa' : null],
        [t('등록 기기'), num(sum('devices'))],
      ], '#/tenancies'),
      tile(t('저장 공간'), bytes(o.stored_bytes), [
        [t('서버 한도'), s.storage_quota_mb ? `${num(s.storage_quota_mb)} MB` : t('한도 없음')],
        s.storage_quota_mb ? [t('사용률'), `${(quota * 100).toFixed(1)}%`, quota > 0.95 ? 'er' : quota > 0.8 ? 'wa' : null] : null,
      ]),
      tile(t('서버'), s.version, [
        [t('가동 시간'), dur(s.now - s.started)],
        [t('인증서'), s.tls_self_signed ? t('자체 서명') : t('운영자 제공')],
        [t('사용자 접속 주소'), s.public_url ? t('설정됨') : t('정하지 않음'), s.public_url ? null : 'wa'],
      ], '#/server-settings'));
    if (tileBox) tileBox.replaceWith(box); else tiles.append(box);
    tileBox = box;
    below.replaceChildren(
      h('div', { class: 'cols' },
        sect(t('서버 속성'), { sub: s.name }, props([
          [t('서버 이름'), s.name],
          [t('서버 ID'), h('span', { class: 'mono' }, s.id)],
          [t('서버 주소'), h('span', { class: 'mono' }, s.domain)],
          [t('버전'), s.version],
          [t('가동 시간'), h('span', { title: tsFull(s.started) }, dur(s.now - s.started))],
          [t('사용자 접속 주소'), s.public_url ? h('span', { class: 'mono' }, s.public_url) : h('span', { class: 'mu' }, t('서버 설정 > 외부 공개에서 정할 수 있습니다'))],
          [t('릴레이 포트'), t('{port} (TLS 1.3 · 평문 TCP{ws} 자동 판별)', { port: s.relay_port, ws: s.websocket ? ' · WebSocket' : '' })],
          [t('인증서'), s.tls_self_signed ? chip('wa', t('자체 서명')) : chip('ok', t('운영자 제공'))],
          [t('SHA-256 지문'), h('span', { class: 'mono' }, s.tls_fingerprint.match(/../g).join(':'))],
          [t('데이터 경로'), h('span', { class: 'mono' }, s.data_dir)],
        ])),
        sect(t('Tenancy 사이의 교환'), { right: h('a', { class: 'lnk', href: '#/server-settings' }, t('변경')) }, props([
          [t('서버 내 Tenancy 간'), tr(EX, s.exchange.cross_tenancy)],
          [t('외부 서버로 보내기'), tr(EX, s.exchange.send_remote)],
          [t('외부 서버에서 받기'), tr(EX, s.exchange.receive_remote)],
          ['', h('span', { class: 'mu' }, t('Tenancy별 예외는 Tenancy 화면에서 정합니다'))],
        ]))),
      sect('Tenancy', { sub: t('{n}개', { n: num(o.tenancies.length) }), right: h('a', { class: 'lnk', href: '#/tenancies' }, t('관리')) }, tenancyGrid(o.tenancies)));
  }
  await draw();
  every(10000, () => draw().catch(fail));
}

function tenancyGrid(items, onRow) {
  return grid('tenancies', [
    { k: 'name', t: t('이름'), v: (x) => [x.name, x.unmigrated ? h('span', {}, ' ', chip('wa', t('구형·기존 사용자'))) : ''] },
    { k: 'id', t: 'Tenancy ID', cls: 'mono' },
    { k: 'online', t: t('접속'), cls: 'num', v: (x) => (x.online ? h('span', {}, h('span', { class: 'dot ok' }), ` ${num(x.online)}`) : h('span', { class: 'mu' }, '0')) },
    { k: 'accounts', t: t('계정'), cls: 'num', v: (x) => `${num(x.accounts)}${x.limits.max_accounts ? ` / ${num(x.limits.max_accounts)}` : ''}` },
    { k: 'pending', t: t('승인 대기'), cls: 'num', v: (x) => (x.pending ? num(x.pending) : h('span', { class: 'mu' }, '0')) },
    { k: 'devices', t: t('기기'), cls: 'num', v: (x) => num(x.devices) },
    { k: 'stored_bytes', t: t('첨부 보관'), cls: 'num', v: (x) => `${bytes(x.stored_bytes)}${x.limits.storage_quota_mb ? ` / ${num(x.limits.storage_quota_mb)} MB` : ''}` },
    { k: 'encryption', t: t('암호화'), v: (x) => tr(ENC, x.encryption) },
    { k: 'admins', t: t('관리자'), cls: 'num', v: (x) => num(x.admins) },
    { k: 'created', t: t('생성'), v: (x) => h('span', { title: tsFull(x.created) }, ts(x.created)) },
    { k: 'act', t: '', sort: false, cls: 'act', v: (x) => h('button', { class: 'btn sm', onclick: () => setTenancy(x.id) }, t('들어가기')) },
  ], items, { onRow, empty: t('Tenancy가 없습니다') });
}

// Exceptions to the server's exchange rules: as the server says, allowed, or not.
function triSelect(v) {
  return h('select', {},
    h('option', { value: '', selected: v == null }, t('서버 설정을 따름')),
    h('option', { value: 'true', selected: v === true }, t('예외로 허용')),
    h('option', { value: 'false', selected: v === false }, t('예외로 금지')));
}
const triValue = (s) => (s.value === '' ? null : s.value === 'true');

// The resource limits form of a tenancy (server admins).
function limitsForm(l = {}) {
  const f = {
    storage_quota_mb: h('input', { type: 'number', min: 0, value: l.storage_quota_mb ?? 0 }),
    max_file_size_mb: h('input', { type: 'number', min: 0, value: l.max_file_size_mb ?? 0 }),
    max_body_kb: h('input', { type: 'number', min: 1, max: 7168, value: l.max_body_kb ?? 1024 }),
    max_accounts: h('input', { type: 'number', min: 0, value: l.max_accounts ?? 0 }),
    archive_files: h('input', { type: 'checkbox', checked: l.archive_files ?? true }),
    cross_tenancy: triSelect(l.cross_tenancy),
    send_remote: triSelect(l.send_remote),
    receive_remote: triSelect(l.receive_remote),
  };
  const nodes = [
    field(t('저장 공간 (MB, 0 = 무제한)'), f.storage_quota_mb),
    field(t('파일 크기 상한 (MB, 0 = 없음)'), f.max_file_size_mb),
    field(t('본문 상한 (KB)'), f.max_body_kb),
    field(t('사용자 수 (0 = 무제한)'), f.max_accounts),
    field('', h('label', { class: 'line' }, f.archive_files, t('감사 사본에 첨부파일 보관 허용'))),
    h('div', { class: 'help' }, t('서버 교환 정책의 예외 (비워 두면 서버 설정을 따름)')),
    field(t('서버 내 Tenancy 간'), f.cross_tenancy),
    field(t('외부 서버로 보내기'), f.send_remote),
    field(t('외부 서버에서 받기'), f.receive_remote),
  ];
  const value = () => ({
    storage_quota_mb: Math.max(0, Number(f.storage_quota_mb.value) || 0),
    max_file_size_mb: Math.max(0, Number(f.max_file_size_mb.value) || 0),
    max_body_kb: Math.max(1, Number(f.max_body_kb.value) || 1024),
    max_accounts: Math.max(0, Number(f.max_accounts.value) || 0),
    archive_files: f.archive_files.checked,
    cross_tenancy: triValue(f.cross_tenancy),
    send_remote: triValue(f.send_remote),
    receive_remote: triValue(f.receive_remote),
  });
  return { nodes, value };
}

async function pTenancies(c, ph) {
  const host = h('div');
  ph.append(h('span', { class: 'sub' }, t('조직마다 Tenancy 하나. 사용자는 클라이언트에서 서버 주소와 Tenancy ID를 입력합니다')));
  let items = [];
  const sec = sect('Tenancy', { tasks: [[t('새 Tenancy…'), create], '-', [t('새로 고침'), () => load().catch(fail)]] },
    h('div', { class: 'tb' }, h('button', { class: 'btn pri', onclick: create }, t('새 Tenancy')), h('span', { class: 'sp' }), h('span', { class: 'mu' }, t('행을 누르면 속성을 엽니다'))), host);
  async function load() {
    items = (await api('GET', '/api/tenancies')).items;
    TENANCIES = items;
    host.replaceChildren(tenancyGrid(items, open));
    sec.sub(t('{n}개', { n: num(items.length) }));
  }
  async function create() {
    const id = h('input', { type: 'text', placeholder: t('예: example'), autocomplete: 'off' });
    const name = h('input', { type: 'text', placeholder: t('예: Example Inc.') });
    const note = h('input', { type: 'text' });
    const user = h('input', { type: 'text', autocomplete: 'off', placeholder: t('비우면 나중에 추가') });
    const pass = h('input', { type: 'password', autocomplete: 'new-password' });
    const lim = limitsForm();
    const ok = await modal(t('새 Tenancy'), [
      field('Tenancy ID', id), h('div', { class: 'help' }, t('2~32자의 영문 소문자, 숫자, -. 사용자가 클라이언트에 입력하고 주소에도 쓰이므로 나중에 바꿀 수 없습니다')),
      field(t('이름'), name), field(t('메모'), note),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('첫 Tenancy 관리자 (선택)')), h('div', { class: 'fsb' }, field(t('관리자 이름'), user), field(t('비밀번호 (8자 이상)'), pass))),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('자원 한도')), h('div', { class: 'fsb' }, lim.nodes)),
    ], {
      ok: t('만들기'),
      wide: true,
      onOk: () => api('POST', '/api/tenancies', {
        id: id.value.trim().toLowerCase(), name: name.value.trim(), note: note.value, limits: lim.value(),
        ...(user.value.trim() ? { admin: { username: user.value.trim(), password: pass.value } } : {}),
      }),
    });
    if (ok) { toast(t('Tenancy를 만들었습니다')); load(); shellTenancies(); }
  }
  async function open(x) {
    const name = h('input', { type: 'text', value: x.name });
    const note = h('textarea', { rows: 2, value: x.note });
    const lim = limitsForm(x.limits);
    const dr = drawer(`${x.name} (${x.id})`, [
      props([
        ['Tenancy ID', h('span', { class: 'mono' }, x.id)],
        [t('생성'), tsFull(x.created)],
        [t('계정 / 기기'), t('{a}명 · {d}대 · 접속 {o}', { a: num(x.accounts), d: num(x.devices), o: num(x.online) })],
        [t('첨부 보관'), bytes(x.stored_bytes)],
        [t('정책'), t('{auth} · 가입 {reg} · {enc}', { auth: tr(AUTH, x.auth), reg: tr(REG, x.registration), enc: tr(ENC, x.encryption) })],
      ], 'one'),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('정보')), h('div', { class: 'fsb' }, field(t('이름'), name), field(t('메모'), note))),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('자원 한도와 교환 예외')), h('div', { class: 'fsb' }, lim.nodes)),
      !x.unmigrated && h('div', { class: 'fs' }, h('div', { class: 'hd' }, t('삭제')),
        h('div', { class: 'fsb' },
          h('div', { class: 'help' }, t('Tenancy를 지우려면 먼저 모든 사용자가 없어야 합니다. "사용자 일괄 삭제"로 계정, 기기, 보관 쪽지를 한 번에 지울 수 있습니다.')),
          h('div', { class: 'line' },
            h('button', { class: 'btn dan', onclick: purge }, t('사용자 일괄 삭제 ({n}명)', { n: num(x.accounts) })),
            h('button', { class: 'btn dan', onclick: del, disabled: x.accounts > 0 }, t('Tenancy 삭제'))))),
    ], [
      h('button', { class: 'btn', onclick: () => setTenancy(x.id) }, t('이 Tenancy 관리하기')), h('span', { class: 'sp' }),
      h('button', { class: 'btn pri', onclick: save }, t('저장')),
      h('button', { class: 'btn', onclick: () => dr.close() }, t('취소')),
    ]);
    async function save() {
      try {
        await api('PATCH', `/api/tenancies/${x.id}`, { name: name.value.trim(), note: note.value, limits: lim.value() });
        toast(t('저장했습니다')); dr.close(); load(); shellTenancies();
      } catch (e) { fail(e); }
    }
    async function purge() {
      const confirmIn = h('input', { type: 'text', autocomplete: 'off', placeholder: x.id });
      let r = null;
      const ok = await modal(t('사용자 일괄 삭제'), [
        h('div', { class: 'warnbox' }, t('{name} Tenancy의 계정 {n}개와 등록된 기기, 받지 못한 쪽지가 모두 삭제됩니다. 되돌릴 수 없습니다.', { name: x.name, n: num(x.accounts) })),
        h('div', {}, t('확인을 위해 Tenancy ID "{id}"를 입력하세요', { id: x.id })),
        confirmIn,
      ], { ok: t('모두 삭제'), danger: true, onOk: async () => { r = await api('POST', `/api/tenancies/${x.id}/purge`, { confirm: confirmIn.value.trim() }); } });
      if (ok && r) { toast(t('계정 {n}개를 삭제했습니다', { n: r.count })); dr.close(); load(); }
    }
    async function del() {
      if (!(await confirmBox(t('Tenancy 삭제'), [t('{name} ({id}) Tenancy를 삭제할까요?', { name: x.name, id: x.id }), h('div', { class: 'warnbox' }, t('Tenancy 관리자, 부서, 감사 로그, 감사 사본이 함께 삭제됩니다. 되돌릴 수 없습니다.'))], t('삭제'), true))) return;
      try { await api('DELETE', `/api/tenancies/${x.id}`); toast(t('삭제했습니다')); dr.close(); load(); shellTenancies(); } catch (e) { fail(e); }
    }
  }
  c.append(sec);
  await load();
}

/// Refreshes the tenancy picker after tenancies change.
function shellTenancies() {
  const sel = document.querySelector('select.tenpick');
  if (!sel) return;
  api('GET', '/api/tenancies').then((r) => {
    TENANCIES = r.items;
    sel.replaceChildren(h('option', { value: '' }, t('서버 전체 (총괄)')), ...r.items.map((x) => h('option', { value: x.id, selected: x.id === TEN }, `${x.name} (${x.id})`)));
  }).catch(fail);
}

// ---------------------------------------------------------------- departments
async function pDepartments(c, ph) {
  const host = h('div');
  ph.append(h('span', { class: 'sub' }, t('사용자는 부서를 여러 개 가질 수 있습니다. 주소 "사용자@부서:tenancy:서버"와 클라이언트의 부서 선택에 쓰입니다')));
  const sec = sect(t('부서'), { tasks: [[t('새 부서…'), () => edit(null)], '-', [t('새로 고침'), () => load().catch(fail)]] },
    h('div', { class: 'tb' }, h('button', { class: 'btn pri', onclick: () => edit(null) }, t('새 부서'))), host);
  async function load() {
    const { items } = await api('GET', '/api/departments');
    sec.sub(t('{n}개', { n: num(items.length) }));
    host.replaceChildren(grid('departments', [
      { k: 'name', t: t('이름') },
      { k: 'id', t: t('부서 ID'), cls: 'mono' },
      { k: 'members', t: t('구성원'), cls: 'num', v: (d) => h('a', { href: `#/accounts?state=all&department=${encodeURIComponent(d.id)}` }, t('{n}명', { n: num(d.members) })) },
      { k: 'note', t: t('메모'), cls: 'trunc', v: (d) => h('span', { title: d.note }, d.note) },
      { k: 'created', t: t('생성'), v: (d) => h('span', { title: tsFull(d.created) }, ts(d.created)) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (d) => [
        h('button', { class: 'btn sm', onclick: () => members(d) }, t('구성원')),
        h('button', { class: 'btn sm', onclick: () => edit(d) }, t('수정')),
        h('button', { class: 'btn sm dan', onclick: () => del(d) }, t('삭제')),
      ] },
    ], items, { empty: t('부서가 없습니다') }));
  }
  async function edit(d) {
    const id = h('input', { type: 'text', value: d?.id || '', placeholder: t('예: developers'), autocomplete: 'off' });
    const name = h('input', { type: 'text', value: d?.name || '', placeholder: t('예: 개발팀') });
    const note = h('input', { type: 'text', value: d?.note || '' });
    const ok = await modal(d ? t('부서 수정') : t('새 부서'), [
      field(t('부서 ID'), id), h('div', { class: 'help' }, t('1~32자의 영문 소문자, 숫자, . _ -. 주소에 쓰입니다') + (d ? t('. 바꾸면 이 부서를 쓰던 주소가 더는 맞지 않습니다') : '')),
      field(t('이름'), name), field(t('메모'), note),
    ], {
      ok: d ? t('저장') : t('만들기'),
      onOk: () => (d
        ? api('PATCH', `/api/departments/${encodeURIComponent(d.id)}`, { id: id.value.trim().toLowerCase(), name: name.value.trim(), note: note.value })
        : api('POST', '/api/departments', { id: id.value.trim().toLowerCase(), name: name.value.trim(), note: note.value })),
    });
    if (ok) { toast(t('저장했습니다')); load(); }
  }
  async function members(d) {
    // Add people by username, remove them from the list.
    const cur = (await api('GET', `/api/accounts?department=${encodeURIComponent(d.id)}&per=500&state=`)).items;
    const remove = new Set();
    const add = h('input', { type: 'text', placeholder: t('사용자 이름, 쉼표로 여러 명') });
    const list = grid('dep-members', [
      { k: 'username', t: t('사용자 이름'), v: (a) => a.username || h('span', { class: 'mu' }, '—') },
      { k: 'name', t: t('표시 이름') },
      { k: 'id', t: t('계정 ID'), cls: 'mono' },
    ], cur, { sel: remove, empty: t('구성원이 없습니다') });
    const ok = await modal(t('{name} 구성원', { name: d.name }), [
      field(t('추가'), add),
      h('div', { class: 'help' }, t('아래 목록에서 체크한 사람은 부서에서 뺍니다')),
      list,
    ], {
      ok: t('적용'),
      wide: true,
      onOk: async () => {
        const names = add.value.split(',').map((x) => x.trim()).filter(Boolean);
        const ids = [];
        for (const n of names) {
          const found = (await api('GET', `/api/accounts?q=${encodeURIComponent(n)}&per=50`)).items.find((a) => (a.username || '').toLowerCase() === n.toLowerCase() || a.id === n);
          if (!found) throw new Error(t('사용자를 찾을 수 없습니다: {n}', { n }));
          ids.push(found.id);
        }
        await api('POST', `/api/departments/${encodeURIComponent(d.id)}/members`, { add: ids, remove: [...remove] });
      },
    });
    if (ok) { toast(t('구성원을 바꿨습니다')); load(); }
  }
  async function del(d) {
    if (!(await confirmBox(t('부서 삭제'), [t('{name} ({id}) 부서를 삭제할까요?', { name: d.name, id: d.id }), t('구성원 {n}명의 계정은 그대로 두고 부서 소속만 지웁니다.', { n: d.members })], t('삭제'), true))) return;
    try { await api('DELETE', `/api/departments/${encodeURIComponent(d.id)}`); toast(t('삭제했습니다')); load(); } catch (e) { fail(e); }
  }
  c.append(sec);
  await load();
}

// ---------------------------------------------------------------- API keys
async function pKeys(c, ph) {
  const host = h('div');
  const warn = h('div');
  ph.append(h('span', { class: 'sub' }, t('스크립트가 사용자 대신 쪽지를 보내고 받는 Bearer 토큰. 사용자는 포털에서 직접 발급할 수 있습니다')));
  const sec = sect(t('API 키'), { tasks: [[t('키 발급…'), create], '-', [t('새로 고침'), () => load().catch(fail)]] },
    h('div', { class: 'tb' }, h('button', { class: 'btn pri', onclick: create }, t('키 발급'))), host);
  async function load() {
    const r = await api('GET', '/api/keys');
    warn.replaceChildren(r.allowed ? '' : h('div', { class: 'warnbox' }, t('이 Tenancy는 API 키를 허용하지 않아 발급된 키도 동작하지 않습니다. '), h('a', { href: '#/policy' }, t('정책 > API 키')), t('에서 켤 수 있습니다.')));
    sec.sub(t('{n}개', { n: num(r.items.length) }), r.allowed ? '' : t(' | 허용 안 됨'));
    host.replaceChildren(grid('keys', [
      { k: 'name', t: t('이름') },
      { k: 'account_name', t: t('계정@@one'), v: (k) => h('a', { href: `#/accounts?state=all&id=${k.account}` }, k.account_name) },
      { k: 'scope', t: t('권한'), sort: false, v: (k) => [k.send && chip('ac', t('보내기')), ' ', k.receive && chip('ok', t('받기'))] },
      { k: 'pending', t: t('받을 쪽지'), cls: 'num', v: (k) => (k.receive ? num(k.pending) : h('span', { class: 'mu' }, '—')) },
      { k: 'last_used', t: t('마지막 사용'), v: (k) => ago(k.last_used) },
      { k: 'last_ip', t: t('마지막 IP'), cls: 'mono' },
      { k: 'created', t: t('발급'), v: (k) => h('span', { title: `${tsFull(k.created)} · ${k.created_by}` }, ts(k.created)) },
      { k: 'id', t: t('키 ID'), cls: 'mono' },
      { k: 'act', t: '', sort: false, cls: 'act', v: (k) => h('button', { class: 'btn sm dan', onclick: () => revoke(k) }, t('폐기')) },
    ], r.items, { empty: t('발급된 키가 없습니다') }));
  }
  async function create() {
    const user = h('input', { type: 'text', placeholder: t('사용자 이름 또는 계정 ID'), autocomplete: 'off' });
    const name = h('input', { type: 'text', placeholder: t('예: 빌드 알림') });
    const send = h('input', { type: 'checkbox', checked: true });
    const recv = h('input', { type: 'checkbox' });
    let r = null;
    const ok = await modal(t('API 키 발급'), [
      field(t('계정@@one'), user), field(t('이름'), name),
      field(t('권한'), h('div', { class: 'checks' },
        h('label', { class: 'line' }, send, t('보내기')),
        h('label', { class: 'line' }, recv, t('받기 — 이 계정 앞으로 온 쪽지를 스크립트가 받아 확인(ack)합니다')))),
    ], {
      ok: t('발급'),
      onOk: async () => {
        const n = user.value.trim();
        const found = (await api('GET', `/api/accounts?q=${encodeURIComponent(n)}&per=50`)).items.find((a) => (a.username || '').toLowerCase() === n.toLowerCase() || a.id === n);
        if (!found) throw new Error(t('계정을 찾을 수 없습니다'));
        r = await api('POST', '/api/keys', { account: found.id, name: name.value.trim(), send: send.checked, receive: recv.checked });
      },
    });
    if (!ok || !r) return;
    await showToken(r.token);
    load();
  }
  async function revoke(k) {
    if (!(await confirmBox(t('API 키 폐기'), [t('{acc}의 "{name}" 키를 폐기할까요?', { acc: k.account_name, name: k.name }), k.receive ? t('이 키가 받지 않은 쪽지 {n}건도 함께 지웁니다.', { n: k.pending }) : ''], t('폐기'), true))) return;
    try { await api('DELETE', `/api/keys/${k.id}`); toast(t('폐기했습니다')); load(); } catch (e) { fail(e); }
  }
  c.append(warn, sec);
  await load();
}

/// Shows a new API key once.
function showToken(token) {
  const tk = h('input', { type: 'text', readonly: true, value: token, class: 'mono wide', onfocus: (e) => e.target.select() });
  return modal(t('API 키'), [
    h('div', { class: 'warnbox' }, t('이 키는 지금 한 번만 보여 줍니다. 안전한 곳에 보관하세요.')),
    tk,
    h('div', { class: 'help' }, t('사용 예: curl -H "Authorization: Bearer <키>" https://<릴레이 주소>/api/v1/me')),
  ], { ok: t('복사하고 닫기'), cancel: t('닫기'), wide: true, onOk: () => navigator.clipboard.writeText(token).then(() => toast(t('복사했습니다'))).catch(() => {}) });
}

// ---------------------------------------------------------------- English dictionary
// Korean source string → English. Keep keys exactly as written in the code.
const EN = {
  // shell, common
  '릴레이 관리자': "Relay Manager", '서버: {name}': "Server: {name}", '서버 전체로 돌아가기': "Back to the whole server",
  '작업할 곳': "Where to work", '작업 위치': "Scope", '서버 전체 (총괄)': "Entire server (server admin)", '메뉴': "Menu",
  '서버 총괄 관리자': "Server administrator", 'Tenancy 관리자': "Tenancy administrator", '총괄': "Server admin", '관리자': "Administrators",
  '로그아웃': "Sign out", '테마: 시스템 설정': "Theme: system", '테마: 밝게': "Theme: light", '테마: 어둡게': "Theme: dark",
  '운영': "Operations", '대시보드': "Dashboard", '접속 현황': "Connections", '사용자': "Users", '계정': "Accounts", '부서': "Departments",
  '기기': "Devices", 'API 키': "API keys", '데이터': "Data", '보관함': "Stored messages", '쪽지 감사': "Message audit", '감사 로그': "Audit log",
  '설정': "Configuration", '정책': "Policy", '서버': "Server", '서버 설정': "Server settings", '서버 감사 로그': "Server audit log",
  '총괄 관리자': "Server administrators", '접속 중': "Online",
  '로그인이 필요합니다': "Please sign in", '복사했습니다': "Copied", '복사': "Copy", '확인': "OK", '취소': "Cancel", '닫기': "Close",
  '속성': "Properties", '작업': "TASKS", '이 화면이 열려 있는 동안 자동으로 갱신합니다': "Refreshes automatically while this page is open",
  '{n}초마다 갱신': "Refreshes every {n}s", '항목이 없습니다': "No items", '모두 선택': "Select all",
  '처음': "First", '이전': "Previous", '다음': "Next", '마지막': "Last", '{page}/{pages}쪽': "Page {page}/{pages}", '쪽당 행 수': "Rows per page", '{n}개씩': "{n} / page",
  '새로 고침': "Refresh", '마지막 갱신 {t}': "Last refreshed {t}",
  // time
  '{n}초': "{n}s", '{n}분': "{n} min", '{h}시간 {m}분': "{h} h {m} min", '{d}일 {h}시간': "{d} d {h} h", '{d} 후': "in {d}", '방금': "just now",
  '{d} 전': "{d} ago", '만료됨': "Expired", '{d} 남음': "{d} left",
  // enumerations
  '활성': "Active", '승인 대기': "Pending approval", '정지@@state': "Disabled", '온라인': "Online", '방해금지': "Do not disturb", '오프라인': "Offline",
  '평문': "Plaintext", '서버 통신 암호화 (TLS)': "Transport encryption (TLS)", '종단간 암호화': "End-to-end encryption",
  '닫힘': "Closed", '승인제': "Approval", '열림': "Open", '사용자 이름 + 비밀번호': "Username + password", '없음 (기기 인증)': "None (device authentication)",
  'TCP 평문': "TCP plaintext", 'WebSocket 평문': "WebSocket plaintext", 'WebSocket · 프록시 TLS': "WebSocket · proxy TLS",
  // sign-in
  '비우면 서버 총괄 관리자': "Blank for the server administrator", '관리자 계정 만들기': "Create administrator", '로그인': "Sign in",
  '비밀번호가 서로 다릅니다': "The passwords do not match", '최초 설정': "Initial setup", '관리자 로그인': "Administrator sign-in",
  '아직 관리자 계정이 없습니다. 지금 만드는 계정이 첫 서버 총괄 관리자가 됩니다. 이 화면은 첫 계정을 만든 뒤 다시 나타나지 않습니다.': "There is no administrator yet. The account you create now becomes the first server administrator. This screen does not appear again once it exists.",
  'Tenancy 관리자는 자기 Tenancy ID를 입력합니다. 서버 총괄 관리자는 비워 둡니다.': "Tenancy administrators enter their Tenancy ID. Server administrators leave it blank.",
  '관리자 이름': "Administrator name", '비밀번호 ({n}자 이상)': "Password ({n}+ characters)", '비밀번호': "Password", '비밀번호 확인': "Confirm password",
  // tenancy dashboard
  '이 Tenancy의 상태와 정책을 한눈에 봅니다': "Status and policy of this tenancy at a glance", '상태': "Status",
  '계정 {a} | 접속 {o} | 마지막 갱신 {t}': "Accounts: {a} | Online: {o} | Last refreshed {t}", '접속': "Online",
  '암호화된 연결': "Encrypted connections", '암호화 안 된 연결': "Unencrypted connections", '암호화': "Encryption", '외부인 가입': "Self sign-up",
  '사용자 수 한도': "User limit", '계정당 기기': "Devices per account", '전달 대기 (기기 단위)': "Pending deliveries (per device)",
  '가장 오래된 쪽지': "Oldest message", '만료 알림 대기': "Pending expiry notices", '보관 시간': "Retention",
  '쪽지 {a}시간 · 첨부 {b}시간': "Messages {a} h · files {b} h", '저장 공간': "Storage", 'Tenancy 한도': "Tenancy quota", '무제한': "Unlimited",
  '사용률': "Usage", 'DB 크기': "Database size", '평문 쪽지 사본': "Copies of plaintext messages",
  '인증서': "Certificate", '자체 서명': "Self-signed", '운영자 제공': "Operator-provided", '서버 속성': "Server properties", '서버 이름': "Server name",
  '서버 ID': "Server ID", '버전': "Version", '가동 시간': "Uptime", '사용자 접속 주소': "Public address for users",
  '서버 설정 > 외부 공개에서 정할 수 있습니다 (총괄 관리자)': "Set it in Server settings > Public access (server administrators)",
  '릴레이 포트': "Relay port", '{port} (TLS 1.3 · 평문 TCP{ws} 자동 판별)': "{port} (TLS 1.3 · plain TCP{ws}, auto-detected)",
  '{n}KB 단위 스트리밍': "Streams in {n} KB chunks", ' · 프록시가 TLS 처리': " · TLS handled by the proxy", 'SHA-256 지문': "SHA-256 fingerprint",
  '데이터 경로': "Data path", '현재 정책': "Current policy", '변경': "Change", '로그온': "Sign-in", '{n}대': "{n} devices", '경유 강제': "Force relay",
  '예 · 외부 LAN 직접 전송 금지': "Yes · no direct LAN transfer to outsiders", '예': "Yes", '아니요': "No", '첨부 한도': "Attachment limits",
  '{n}개 · 파일당 {s}': "{n} files · {s} per file", '첨부 중계 안 함': "Attachments not relayed", '보관': "Retention", '연락처': "Contacts",
  '전체 공개': "Everyone", '검색한 사용자만': "Looked-up users only", 'Tenancy 속성': "Tenancy properties", '사용자 안내': "For users",
  '클라이언트에서 릴레이 서버 주소와 함께 이 Tenancy ID를 입력합니다': "Users enter this Tenancy ID together with the relay address in the client",
  '주소 형식': "Address format", '사용자 이름': "Username",
  'Tenancy를 지원하지 않는 구형 클라이언트와 기존 사용자가 들어오는 곳입니다. 계정 화면에서 사용자를 다른 Tenancy로 옮길 수 있습니다(총괄 관리자)': "Where older clients without tenancy support and existing users land. Server administrators can move users to another tenancy on the Accounts page.",
  '{v} (한도 없음)': "{v} (no quota)", '{a} / {b}명': "{a} / {b} users", '없음': "None", '파일 크기 상한': "Maximum file size", '본문 상한': "Maximum body size",
  '자원 한도는 서버 총괄 관리자가 정합니다': "Resource limits are set by the server administrator", '일별 처리량': "Daily traffic", 'UTC, 최근 30일': "UTC, last 30 days",
  '날짜': "Date", '쪽지': "Messages", '기기 전달': "Device deliveries", '첨부 용량': "Attachment bytes", '만료 (기기 단위)': "Expired (per device)",
  '아직 기록이 없습니다': "Nothing recorded yet",
  // connections
  '지금 릴레이에 접속해 있는 기기': "Devices connected to the relay right now", '접속 중인 기기': "Connected devices",
  '필터: 이름, 사용자 이름, 계정·기기 ID, IP, 앱': "Filter: name, username, account/device ID, IP, app", '이름': "Name", '(이름 없음)': "(no name)",
  '기기 ID': "Device ID", '계정 ID': "Account ID", '연결': "Transport", '앱': "App", '접속 시각': "Connected at", '접속 시간': "Duration", '끊기': "Disconnect",
  '조건에 맞는 연결이 없습니다': "No matching connections", '접속 중인 기기가 없습니다': "No devices are connected",
  '전체 {n}개 | 암호화 안 됨 {p}개': "Total: {n} | Unencrypted: {p}", '전체 {n}개 연결 · 암호화 안 된 연결 {p}개': "{n} connections · {p} unencrypted",
  '연결 끊기': "Disconnect", '{name} 기기의 연결을 끊을까요?': "Disconnect the device {name}?",
  '클라이언트는 곧바로 다시 접속할 수 있습니다. 막으려면 기기를 정지하세요.': "The client can reconnect right away. Disable the device to keep it out.",
  '연결을 끊었습니다': "Disconnected",
  // accounts
  '승인': "Approve", '계정 승인': "Approve accounts", '선택한 {n}개 계정을 승인할까요?': "Approve the {n} selected accounts?", '{n}개 계정을 승인했습니다': "Approved {n} accounts",
  '정지 해제': "Enable", '계정 정지 해제': "Enable accounts", '선택한 {n}개 계정의 정지를 해제할까요?': "Enable the {n} selected accounts?", '{n}개 계정의 정지를 해제했습니다': "Enabled {n} accounts",
  '정지': "Disable", '계정 정지': "Disable accounts", '선택한 {n}개 계정을 정지할까요?': "Disable the {n} selected accounts?", '{n}개 계정을 정지했습니다': "Disabled {n} accounts",
  '삭제': "Delete", '계정 삭제': "Delete account", '선택한 {n}개 계정을 삭제할까요?': "Delete the {n} selected accounts?", '{n}개 계정을 삭제했습니다': "Deleted {n} accounts",
  '모든 부서': "All departments", '다른 Tenancy로 옮기기': "Move to another tenancy",
  '로그온 방식이 "없음"이라 기기 하나가 계정 하나입니다': "Sign-in is \"None\", so each device is its own account",
  '새 계정…': "New account…", '새 계정': "New account", '필터: 사용자 이름, 표시 이름, ID, 메모': "Filter: username, display name, ID, note",
  '{n}개 선택': "{n} selected", '선택한 계정에 적용:': "Apply to selected:", '전체 {n}': "All {n}", '활성 {n}': "Active {n}", '승인 대기 {n}': "Pending {n}", '정지 {n}': "Disabled {n}",
  '표시 이름': "Display name", '받을 쪽지': "Pending", '보낸 첨부 보관': "Stored uploads", '저장 한도': "Quota", '마지막 접속': "Last seen", '생성': "Created", '메모': "Note",
  '승인을 기다리는 가입 신청이 없습니다': "No sign-ups are waiting for approval", '계정이 없습니다': "No accounts",
  '{f} | 조건에 맞는 {n}개': "{f} | {n} matching", '전체 계정': "All accounts", '조건에 맞는 {n}개': "{n} matching",
  '* 계정별로 따로 정한 값': "* set for this account", '선택은 현재 쪽에만 적용됩니다': "Selection applies to the current page only", ' 외 {n}개': " and {n} more",
  '계정과 등록된 기기, 받지 못한 쪽지가 모두 삭제됩니다. 되돌릴 수 없습니다.': "The accounts, their devices and undelivered messages are all deleted. This cannot be undone.",
  '접속 중인 기기는 곧바로 연결이 끊깁니다.': "Connected devices are disconnected immediately.", '먼저 Tenancy를 만드세요': "Create a tenancy first",
  '선택한 {n}개 계정을 옮깁니다.': "Moving the {n} selected accounts.", '옮길 Tenancy': "Destination tenancy",
  '계정, 등록된 기기(기기 비밀값 포함), 아직 받지 못한 쪽지와 첨부파일이 함께 옮겨집니다. 구형 클라이언트는 그대로 접속해도 새 Tenancy로 연결되고, 신형 클라이언트는 저장된 Tenancy ID를 자동으로 바꿉니다.': "Accounts move together with their devices (including device secrets) and undelivered messages and files. Older clients keep connecting as before and land in the new tenancy; newer clients update their saved Tenancy ID automatically.",
  '옮긴 계정은 이 Tenancy의 연락처에서 사라집니다. 같은 사용자 이름이나 기기가 이미 있는 계정은 옮기지 않습니다.': "Moved accounts disappear from this tenancy's contacts. Accounts whose username or device already exists there are not moved.",
  '옮기기': "Move", '{n}개 계정을 {to} Tenancy로 옮겼습니다': "Moved {n} accounts to the {to} tenancy", '옮기지 못한 계정': "Accounts not moved",
  '비우면 사용자 이름': "Blank: the username", '정책 기본값 ({n})': "Policy default ({n})", '정책 기본값': "Policy default",
  '지금 로그온 방식은 "없음"입니다. 사용자 이름으로 만든 계정은 로그온 방식이 "비밀번호"일 때 쓸 수 있습니다.': "Sign-in is currently \"None\". Accounts created with a username can be used when sign-in is \"Password\".",
  '기기 수 한도': "Device limit", '저장 한도 (MB)': "Quota (MB)", '만들기': "Create", '계정을 만들었습니다': "Account created", '바꿀 때만 입력': "Only to change it",
  '정책 기본값 (무제한)': "Policy default (unlimited)", '부서가 없습니다. 부서 화면에서 만들 수 있습니다': "No departments. Create them on the Departments page",
  '등록된 기기가 없습니다': "No registered devices", '{n}건 (기기 단위)': "{n} (per device)", '설정됨': "Set", '계정 정보': "Account",
  '정지하거나 승인 대기로 돌리면 접속 중인 기기의 연결이 끊깁니다': "Disabling or returning to pending disconnects its connected devices",
  '비우면 비밀번호 로그온을 쓸 수 없습니다': "Without a username, password sign-in is not possible",
  '기기가 접속하면 기기에서 보낸 이름으로 바뀝니다': "Replaced by the name a device sends when it connects",
  '여러 부서에 속할 수 있습니다. 주소의 부서 부분과 클라이언트의 부서 선택에 쓰입니다': "An account can belong to several departments. Used in the department part of addresses and the client's department picker",
  '새 비밀번호': "New password", '비우면 정책 기본값. 1~9999': "Blank: policy default. 1–9999",
  '이 계정이 보낸 첨부파일의 서버 보관 용량. 비우면 정책 기본값, 0은 무제한': "Server storage for files this account sends. Blank: policy default; 0: unlimited",
  '기기 관리': "Manage devices", '저장': "Save", '저장했습니다': "Saved", '{name} 계정을 삭제할까요?': "Delete the account {name}?",
  '등록된 기기 {d}대와 받지 못한 쪽지 {p}건이 함께 삭제됩니다. 되돌릴 수 없습니다.': "Its {d} registered devices and {p} undelivered messages are deleted too. This cannot be undone.",
  '삭제했습니다': "Deleted",
  // devices
  '필터: 이름, 계정, 기기 ID, IP, 호스트': "Filter: name, account, device ID, IP, host", '접속 중 {n}': "Online {n}", '받을 쪽지 있음 {n}': "With pending {n}",
  '기기 이름': "Device name", '계정@@one': "Account", '계정 상태': "Account state", '상태 설정': "Presence", '호스트': "Host", '마지막 IP': "Last IP",
  'E2EE 키 지문': "E2EE key fingerprint", '수신 한도': "Receive limits", '{n}개 · {s}': "{n} files · {s}", '등록': "Registered", '등록 해제': "Unregister",
  '기기가 없습니다': "No devices", '전체 {a} | 접속 중 {o} | 조건에 맞는 {n}개': "Total: {a} | Online: {o} | {n} matching",
  '기기 정지': "Disable device", '{name} 기기를 정지할까요?': "Disable the device {name}?",
  '정지된 기기는 로그온할 수 없고, 연락처에서도 빠집니다.': "A disabled device cannot sign in and is removed from contacts.",
  '기기 정지 해제': "Enable device", '{name} 기기의 정지를 풀까요?': "Enable the device {name}?", '기기 등록 해제': "Unregister device",
  '{name} 기기의 등록을 해제할까요?': "Unregister the device {name}?",
  '이 기기가 받지 못한 쪽지 {n}건이 삭제됩니다. 기기는 다음 로그온 때 새 기기로 다시 등록됩니다 (기기 수 한도 적용).': "Its {n} undelivered messages are deleted. The device registers again as a new device at its next sign-in (the device limit applies).",
  '처리했습니다': "Done",
  // stored messages
  '평문 쪽지는 행을 눌러 내용과 첨부파일을 열람할 수 있습니다. 열람 기록은 감사 로그에 남습니다': "Click a plaintext message to read it and its attachments. Each view is recorded in the audit log",
  '보관 중인 쪽지': "Stored messages", '만료 처리 지금 실행': "Run expiry now", '필터: 보낸 사람, 받는 사람, ID': "Filter: sender, recipient, ID",
  '보관 시작': "Stored at", '보낸 사람': "From", '받는 사람': "To", '전달 대기': "Pending", '{a} / {b} 기기': "{a} / {b} devices", '첨부': "Files",
  '{n}개 (삭제됨)': "{n} (deleted)", '{n}개': "{n}", '크기': "Size", '본문': "Body", '첨부 만료': "Files expire", '쪽지 만료': "Message expires",
  '보관 중인 쪽지가 없습니다': "No stored messages", '{n}건 | 첨부 {b}': "{n} messages | files {b}",
  '조건에 맞는 {n}건 · 첨부 {b} · 전달 대기 {p}건 (기기 단위)': "{n} matching · files {b} · {p} pending deliveries (per device)",
  '보관 쪽지 삭제': "Delete stored message", '{name}님이 보낸 쪽지를 지금 삭제할까요?': "Delete the message from {name} now?",
  '아직 받지 못한 기기 {n}대는 이 쪽지를 받을 수 없게 되며, 만료 알림도 남지 않습니다.': "The {n} devices that have not received it will never get it, and no expiry notice is left.",
  '첨부 {f}건 · 쪽지 {m}건 만료 처리 (만료 알림 {n}건)': "Expired {f} files · {m} messages ({n} expiry notices)",
  '(본문 없음)': "(no body)", '그룹': "Group", '그룹 구성원': "Group members", '파일 이름': "File name", '다운로드': "Download", '보관 안 됨': "Not kept",
  '첨부파일 없음': "No attachments", '보관 쪽지 · {name}': "Stored message · {name}", '삭제됨': "Deleted", '쪽지 ID': "Message ID", '내용': "Content",
  '종단간 암호화된 쪽지입니다. 서버는 본문, 파일 이름, 첨부파일 내용을 볼 수 없습니다.': "This message is end-to-end encrypted. The server cannot see its body, file names or attachments.",
  '첨부파일': "Attachments", '전달 상태': "Delivery", '{a}/{b} 기기 수신': "{a}/{b} devices received", '기기@@one': "Device", '수신': "Received",
  '받음': "Received", '대기': "Waiting", '이 열람은 감사 로그에 기록되었습니다': "This view was recorded in the audit log",
  // message audit
  '본문, 보낸 사람, 받는 사람, 파일 이름': "Body, sender, recipient, file name", '조회': "Search",
  '조건을 정하고 조회하세요. 조회할 때마다 감사 로그에 기록됩니다.': "Set the conditions and search. Every search is recorded in the audit log.",
  '종단간 암호화가 아닌 쪽지의 사본 (전달 후에도 보관)': "Copies of messages that are not end-to-end encrypted (kept after delivery)",
  '먼저 조회하세요': "Search first", 'CSV 내보내기 (최대 2만 건)': "Export CSV (up to 20,000)", '기간': "Period", '보낸 시각': "Sent",
  '{n}개 (미보관)': "{n} (not kept)", '조건에 맞는 쪽지가 없습니다': "No matching messages",
  '조건에 맞는 {n}건 | 전체 보관 {a}건': "{n} matching | {a} archived", '조건에 맞는 {n}건 · 전체 보관 {a}건': "{n} matching · {a} archived in total",
  '감사 보관이 꺼져 있어 새 쪽지는 보관되지 않습니다. ': "Audit archiving is off, so new messages are not archived. ", '정책 > 쪽지 감사': "Policy > Message audit",
  '에서 켤 수 있습니다. 지금 서버에 맡겨진 쪽지는 "보관함"에서 열람할 수 있습니다.': " can turn it on. Messages currently held by the server can be viewed under \"Stored messages\".",
  '현재 암호화 단계가 종단간 암호화라서 새 쪽지는 서버가 내용을 볼 수 없어 보관되지 않습니다.': "Encryption is end-to-end, so the server cannot read new messages and does not archive them.",
  '쪽지 감사 · {name}': "Message audit · {name}", ' ({n}대)': " ({n} devices)", ' · 첨부파일은 보관하지 않도록 설정되어 있었습니다': " · attachments were set not to be kept",
  '감사 사본 삭제': "Delete audit copy",
  '이 쪽지의 감사 사본(본문과 보관된 첨부파일)을 삭제할까요? 삭제 기록은 감사 로그에 남습니다.': "Delete the audit copy of this message (body and kept attachments)? The deletion is recorded in the audit log.",
  // audit actions
  '최초 관리자 생성': "First administrator created", '관리자 로그인 실패': "Administrator sign-in failed", '관리자 추가': "Add administrator",
  '관리자 삭제': "Delete administrator", '관리자 비밀번호 변경': "Administrator password changed", '정책 변경': "Policy changed", '계정 생성': "Account created",
  '계정 수정': "Account updated", '가입': "Sign-up", '기기 추가': "Device added", '기기 계정 이동': "Device moved to account", '만료 처리 실행': "Expiry run",
  '로그인 잠금': "Sign-in locked", '보관 쪽지 열람': "Stored message viewed", '보관 첨부 다운로드': "Stored file downloaded", '쪽지 감사 조회': "Message audit searched",
  '쪽지 감사 열람': "Audit copy viewed", '감사 첨부 다운로드': "Audit file downloaded", '포털 로그인': "Portal sign-in", '포털 로그인 실패': "Portal sign-in failed",
  '포털 비밀번호 변경': "Portal password changed", '포털 이름 변경': "Portal name changed", '포털 기기 연결 끊기': "Portal device disconnected",
  '포털 기기 등록 해제': "Portal device unregistered", '서버 설정 변경': "Server settings changed", 'Tenancy 생성': "Tenancy created", 'Tenancy 수정': "Tenancy updated",
  'Tenancy 삭제': "Delete tenancy", 'Tenancy 사용자 일괄 삭제': "Tenancy users purged", '사용자 일괄 삭제': "Delete all users", '계정 Tenancy 이동': "Account moved to tenancy",
  '다른 Tenancy에서 옮겨 옴': "Moved in from another tenancy", '다른 Tenancy로 옮겨 감': "Moved out to another tenancy", '부서 생성': "Department created",
  '부서 수정': "Edit department", '부서 삭제': "Delete department", '부서 구성원 변경': "Department members changed", 'API 키 발급': "Issue API key",
  'API 키 폐기': "Revoke API key", '릴레이 수신 설정': "Relay receive setting",
  '관리자 추가@@ev': "Administrator added", '관리자 삭제@@ev': "Administrator deleted", '계정 삭제@@ev': "Account deleted", '계정 승인@@ev': "Account approved",
  '계정 정지 해제@@ev': "Account enabled", '계정 정지@@ev': "Account disabled", '기기 정지@@ev': "Device disabled", '기기 정지 해제@@ev': "Device enabled",
  '기기 등록 해제@@ev': "Device unregistered", '연결 끊기@@ev': "Device disconnected", '보관 쪽지 삭제@@ev': "Stored message deleted",
  '감사 사본 삭제@@ev': "Audit copy deleted", 'Tenancy 삭제@@ev': "Tenancy deleted", '사용자 일괄 삭제@@ev': "All users deleted",
  '부서 수정@@ev': "Department updated", '부서 삭제@@ev': "Department deleted", 'API 키 발급@@ev': "API key issued", 'API 키 폐기@@ev': "API key revoked",
  // audit page
  '작업 종류': "Action type", 'CSV 내보내기 (최근 2만 건)': "Export CSV (latest 20,000)", '필터: 주체, 내용': "Filter: actor, detail", '모든 작업': "All actions",
  '시각': "Time", '주체': "Actor", '작업@@event': "Action", '기록이 없습니다': "No records",
  '조건에 맞는 {n}건 | 최신순': "{n} matching | newest first", '조건에 맞는 {n}건 · 최신순': "{n} matching · newest first",
  // policy
  '연락처 공개 범위': "Contact visibility", '전체 공개 — 모든 사용자가 서로 보임': "Everyone — all users see each other",
  '검색 — 주고받았거나 직접 찾은 사용자만': "Lookup — only users you have exchanged with or looked up",
  '사용자가 많은 공개 Tenancy라면 "검색"을 권장합니다. 다른 Tenancy의 사용자는 어느 쪽이든 보이지 않습니다': "\"Lookup\" is recommended for large public tenancies. Users of other tenancies are never listed",
  '컴퓨터 이름 공개': "Show computer names", '연락처에 기기의 컴퓨터(호스트) 이름 포함': "Include each device's computer (host) name in contacts",
  '가입 및 로그온': "Sign-up and sign-in", '로그온 방식': "Sign-in method", '없음 — 기기 ID와 기기 비밀값만': "None — device ID and device secret only",
  '바꾸면 접속 중인 모든 기기의 연결이 끊기고, 다시 로그온해야 합니다': "Changing it disconnects every connected device; they must sign in again",
  '"없음"이면 기기 하나가 계정 하나가 됩니다': "With \"None\", each device becomes its own account",
  '닫힘 — 관리자가 만든 계정만 (기본)': "Closed — only accounts created by administrators (default)", '승인제 — 신청을 받고 관리자가 승인': "Approval — users apply, administrators approve",
  '열림 — 누구나 바로 가입 (공개 서버용)': "Open — anyone can sign up (public servers)", '계정당 기기 수': "Devices per account", '대': "devices",
  '계정마다 따로 정할 수 있습니다. 최대 9999대. API 키 기기는 세지 않습니다': "Can be set per account. Up to 9999. API key devices are not counted",
  '최소 비밀번호 길이': "Minimum password length", '자': "characters", '관리자 계정은 8자 이상': "At least 8 for administrators",
  '로그인 실패 허용': "Allowed sign-in failures", '회': "attempts", '사용자 이름별·기기별·API 키별로 셉니다': "Counted per username, per device and per API key",
  '잠금 시간': "Lockout duration", '분': "min", '경로': "Routing", '릴레이 경유 강제': "Force relay",
  '이 Tenancy 연락처에 있는 사람에게는 LAN에서 보여도 릴레이로 보냄': "Send through the relay to contacts of this tenancy even when they are visible on the LAN",
  '외부 LAN 직접 전송': "Direct LAN transfer to outsiders", '이 Tenancy를 쓰지 않는 LAN 사용자에게 직접 보내는 것도 금지': "Also forbid sending directly to LAN users who are not on this tenancy",
  '경유 강제일 때만 적용됩니다. 서버는 LAN 전송을 볼 수 없으므로 클라이언트가 지킵니다': "Applies only with Force relay. The server cannot see LAN transfers, so clients enforce it",
  '암호화 단계': "Encryption level", '평문 — 암호화 없음': "Plaintext — no encryption",
  '서버 통신 암호화 — TLS 필수, 서버에는 평문 저장': "Transport encryption — TLS required, stored as plaintext on the server",
  '종단간 암호화 — TLS 필수, 서버는 내용을 볼 수 없음': "End-to-end encryption — TLS required, the server cannot read content",
  '평문이 아닌 단계로 바꾸면 평문으로 접속한 기기의 연결이 끊깁니다. 종단간 암호화에서는 API 키를 쓸 수 없습니다': "Leaving plaintext disconnects devices connected without encryption. API keys cannot be used with end-to-end encryption",
  'LAN 직접 전송에는 적용되지 않습니다': "Does not apply to direct LAN transfers", '전송 한도 (릴레이를 거칠 때만)': "Transfer limits (relayed messages only)",
  '첨부파일 수': "Attachments per message", '개': "files", '0이면 첨부파일을 중계하지 않습니다': "0: attachments are not relayed", '파일당 최대 크기': "Maximum size per file",
  '0이면 첨부파일을 중계하지 않습니다. E2EE에서는 암호문 크기 기준. 서버 총괄 관리자가 정한 상한을 넘을 수 없습니다': "0: attachments are not relayed. With E2EE the ciphertext size counts. Cannot exceed the cap set by the server administrator",
  '쪽지 보관 시간': "Message retention", '시간': "hours",
  '지나면 삭제하고, 받지 못한 기기에는 보낸 사람별 만료 개수만 알립니다. 계정의 모든 기기가 받아야 전달 완료로 봅니다': "Deleted afterwards; devices that missed them only learn how many expired per sender. Delivery completes when every device of the account has received it",
  '첨부파일 보관 시간': "Attachment retention", '쪽지 보관 시간보다 길게 정해도 쪽지 보관 시간으로 맞춰집니다': "Capped at the message retention",
  '계정별 저장 한도': "Storage quota per account",
  '보낸 계정 기준. 0이면 무제한. 계정마다 따로 정할 수 있습니다. Tenancy 전체 한도는 서버 총괄 관리자가 정합니다': "Charged to the sender. 0: unlimited. Can be set per account. The tenancy-wide quota is set by the server administrator",
  '다른 Tenancy와 주고받기': "Exchange with other tenancies", '같은 서버로 보내기': "Send within this server",
  '이 서버의 다른 Tenancy로 쪽지와 첨부파일을 보낼 수 있음': "Can send messages and files to other tenancies on this server", '같은 서버에서 받기': "Receive within this server",
  '이 서버의 다른 Tenancy에서 보낸 쪽지와 첨부파일을 받음': "Receives messages and files from other tenancies on this server", '외부 서버로 보내기': "Send to other servers",
  '다른 서버의 Tenancy로 보낼 수 있음': "Can send to tenancies on other servers",
  '서버 사이 전송은 아직 지원하지 않습니다. 정책만 미리 정해 둘 수 있습니다': "Server-to-server transfer is not supported yet; the policy can be set in advance",
  '외부 서버에서 받기': "Receive from other servers", '다른 서버의 Tenancy에서 보낸 쪽지를 받음': "Receives messages from tenancies on other servers", '허용 목록': "Allow list",
  '한 줄에 하나씩 "tenancy:서버". 서버를 빼면 이 서버, Tenancy 자리에 *를 쓰면 그 서버 전체. 비어 있지 않으면 목록에 있는 곳하고만 주고받습니다': "One \"tenancy:server\" per line. Without a server it means this server; * as the tenancy means the whole server. When not empty, exchange only with the listed places",
  '차단 목록': "Deny list", '같은 형식. 여기 있으면 허용 목록과 관계없이 막습니다': "Same format. Entries here are blocked regardless of the allow list",
  'API 키 허용': "Allow API keys", '사용자가 스크립트용 API 키(Bearer 토큰)를 발급해 쪽지를 보내고 받을 수 있음': "Users can issue API keys (Bearer tokens) for scripts to send and receive messages",
  '사용자 포털과 "API 키" 화면에서 발급합니다. 종단간 암호화 단계에서는 켤 수 없습니다. 끄면 발급된 키도 모두 막힙니다': "Issued in the user portal and on the \"API keys\" page. Cannot be enabled with end-to-end encryption. Turning it off blocks all issued keys",
  '감사 보관': "Audit archiving", '종단간 암호화가 아닌 쪽지의 사본을 전달 후에도 보관': "Keep copies of messages that are not end-to-end encrypted, even after delivery",
  '사용자에게는 알리지 않습니다. 조직의 정보 보호 방침에 따라 쓰세요': "Users are not notified. Use it in line with your organization's privacy policy",
  '켠 뒤부터 릴레이를 거치는 쪽지가 보관됩니다. 다른 Tenancy와 주고받은 쪽지도 포함됩니다. LAN 직접 전송은 서버를 거치지 않아 보관되지 않습니다': "Relayed messages are archived from the moment it is on, including those exchanged with other tenancies. Direct LAN transfers bypass the server and are not archived",
  '첨부파일 보관': "Archive attachments", '감사 사본에 첨부파일도 함께 보관': "Keep attachments with audit copies",
  '디스크를 많이 쓸 수 있습니다. 서버 총괄 관리자가 막아 둘 수 있습니다. 끄면 파일 이름과 크기만 남습니다': "Can use a lot of disk. The server administrator may disallow it. When off, only file names and sizes are kept",
  '감사 보관 기간': "Audit retention", '일': "days", '지나면 사본과 첨부파일을 삭제합니다': "Copies and attachments are deleted afterwards", '기록': "Logging",
  '감사 로그 보관': "Audit log retention",
  // server settings
  '클라이언트와 이 관리 화면에 표시됩니다': "Shown in clients and in this console", '서버 주소 (주소 체계)': "Server domain (addresses)",
  '주소 "사용자@tenancy:서버"의 서버 부분. 비우면 사용자 접속 주소의 호스트, 그것도 없으면 인증서 이름을 씁니다': "The server part of \"user@tenancy:server\" addresses. Blank: the host of the public address, or else the certificate name",
  '서버 전체 저장 한도': "Server-wide storage quota",
  '모든 Tenancy의 첨부파일 합계. 0이면 무제한. Tenancy별 한도는 Tenancy 화면에서 정합니다': "Total attachments of all tenancies. 0: unlimited. Per-tenancy quotas are set on the Tenancy page",
  'Tenancy 사이의 교환': "Exchange between tenancies", '서버 내 Tenancy 간': "Between tenancies on this server",
  '이 서버의 Tenancy끼리 쪽지와 첨부파일을 주고받을 수 있음': "Tenancies on this server can exchange messages and files",
  '양쪽 Tenancy도 각자 정책에서 허용해야 합니다. Tenancy별 예외는 Tenancy 화면에서 정합니다': "Both tenancies must allow it in their policies too. Per-tenancy exceptions are set on the Tenancy page",
  '다른 서버로 보내는 것을 허용': "Allow sending to other servers", '다른 서버에서 오는 것을 허용': "Allow receiving from other servers",
  '외부 공개 (HTTPS 프록시 · WebSocket)': "Public access (HTTPS proxy · WebSocket)",
  '사용자에게 안내할 주소. 예: https://relay.example.com (Cloudflare Tunnel 등) 또는 relay.example.com:45443 (TCP 직접)': "The address you give users, e.g. https://relay.example.com (Cloudflare Tunnel etc.) or relay.example.com:45443 (direct TCP)",
  '사용자 웹 포털': "User web portal", '릴레이 주소를 브라우저로 열면 사용자가 자기 계정을 관리하는 화면을 보여 줌': "Opening the relay address in a browser shows a page where users manage their own account",
  '내 정보, 비밀번호 변경, 기기 관리, API 키, 내 활동 기록. 비밀번호 계정만 로그인할 수 있고, 암호화된 연결에서만 로그인됩니다. 스크립트용 API(/api/v1)는 꺼도 동작합니다': "Profile, password change, devices, API keys and activity. Only password accounts can sign in, and only over encrypted connections. The script API (/api/v1) works even when this is off",
  'WebSocket 연결': "WebSocket connections", '릴레이 포트에서 WebSocket(HTTP 업그레이드)도 받음': "Also accept WebSocket (HTTP upgrade) on the relay port",
  'Cloudflare Tunnel처럼 HTTP/HTTPS만 통과시키는 프록시를 거칠 때 필요합니다. 터널 서비스 주소는 http://localhost:45443': "Needed behind proxies that pass only HTTP/HTTPS, such as Cloudflare Tunnel. The tunnel service address is http://localhost:45443",
  '프록시가 TLS 처리': "TLS handled by proxy", '프록시가 HTTPS로 받았다고 표시한 WebSocket 연결을 암호화된 연결로 인정': "Treat WebSocket connections the proxy marks as HTTPS as encrypted",
  '앞단 프록시가 HTTPS를 처리할 때만 켜세요. 켜면 프록시가 보낸 접속자 IP(CF-Connecting-IP / X-Forwarded-For)를 기록합니다': "Turn on only when a front proxy terminates HTTPS. The client IP sent by the proxy (CF-Connecting-IP / X-Forwarded-For) is then recorded",
  'X-Forwarded-Proto: https 또는 CF-Visitor 헤더로 판단합니다': "Decided by the X-Forwarded-Proto: https or CF-Visitor header", '스트리밍 단위': "Streaming chunk size",
  '첨부파일을 이 크기의 WebSocket 메시지로 나눠 보냅니다. HTTP 요청 하나로 올리지 않으므로 Cloudflare의 100MB 업로드 제한에 걸리지 않습니다. 기본 256KB': "Attachments are sent as WebSocket messages of this size rather than one HTTP request, so Cloudflare's 100 MB upload limit does not apply. Default 256 KB",
  '연결 및 관리': "Connections and administration", '무응답 연결 끊기': "Idle connection timeout", '초': "s", '클라이언트는 30초마다 ping을 보냅니다': "Clients ping every 30 seconds",
  '관리자 세션 유지': "Administrator session lifetime", '마지막 사용 시각부터. 모든 관리자에게 적용됩니다': "From the last use. Applies to all administrators",
  '관리자 로그인 실패 허용': "Allowed administrator sign-in failures",
  '관리자 화면 로그인에만 적용됩니다. 사용자 로그인은 Tenancy 정책을 따릅니다': "Applies to console sign-in only. User sign-in follows the tenancy policy",
  '관리자 잠금 시간': "Administrator lockout duration", '서버 감사 로그 보관': "Server audit log retention",
  'Tenancy 생성·삭제, 서버 설정, 총괄 관리자 기록': "Tenancy creation and deletion, server settings, server administrator activity",
  '이 Tenancy의 정책입니다. 저장하면 접속 중인 클라이언트에 바로 알립니다': "The policy of this tenancy. Connected clients are notified as soon as it is saved",
  '자원 한도': "Resource limits", '서버 총괄 관리자가 정합니다': "Set by the server administrator", '사용자 수': "Users", '{n}명': "{n} users",
  '감사 첨부 보관': "Audit attachments", '허용': "Allowed", '불가': "Not allowed",
  '첨부파일 보관 시간이 쪽지 보관 시간보다 깁니다. 저장하면 쪽지 보관 시간으로 맞춰집니다.': "Attachment retention is longer than message retention. It will be capped at message retention when saved.",
  '외부인 가입이 "열림"입니다. 이 Tenancy에 접속할 수 있는 누구나 계정을 만들 수 있습니다.': "Self sign-up is \"Open\". Anyone who can reach this tenancy can create an account.",
  '암호화가 "평문"입니다. 신뢰할 수 있는 내부망에서만 쓰세요.': "Encryption is \"Plaintext\". Use it only on a trusted internal network.",
  '감사 보관이 켜져 있지만 암호화 단계가 종단간 암호화라서 쪽지 내용은 보관되지 않습니다.': "Audit archiving is on, but encryption is end-to-end, so message content is not archived.",
  '종단간 암호화 단계에서는 API 키를 쓸 수 없어 저장하면 꺼집니다.': "API keys cannot be used with end-to-end encryption and will be turned off when saved.",
  '로그온 방식 "없음" + 가입 "열림": 서버에 닿는 모든 기기가 바로 사용자가 됩니다.': "Sign-in \"None\" + sign-up \"Open\": every device that reaches the server becomes a user at once.",
  '서버 전체에 적용됩니다. 사용자 정책은 Tenancy마다 따로 정합니다': "Applies to the whole server. User policy is set per tenancy",
  '서버 내 Tenancy 간 전송이 켜져 있습니다. 실제로 주고받으려면 양쪽 Tenancy도 정책에서 허용해야 합니다.': "Exchange between tenancies is on. Both tenancies must also allow it in their policies.",
  '되돌리기': "Revert", '켬': "On", '끔': "Off", '(없음)': "(none)", '변경 {n}건': "{n} changes", '변경 사항 없음': "No changes", '설정 저장': "Save settings",
  '저장했습니다 ({n}건)': "Saved ({n} changes)",
  // administrators
  '관리자 추가…': "Add administrator…", '나': "me", '마지막 로그인': "Last sign-in",
  '{name} 관리자를 삭제할까요? 로그인 중이면 바로 로그아웃됩니다.': "Delete the administrator {name}? They are signed out at once if signed in.",
  '비밀번호 (8자 이상)': "Password (8+ characters)", '추가': "Add", '추가했습니다': "Added", '새 비밀번호가 서로 다릅니다': "The new passwords do not match",
  '비밀번호를 바꿨습니다': "Password changed", '내 비밀번호 변경': "Change my password", '현재 비밀번호': "Current password", '새 비밀번호 확인': "Confirm new password",
  '관리 화면은 기본적으로 서버 자신(127.0.0.1)에만 열려 있습니다. 원격에서 쓰려면 HTTPS 리버스 프록시나 SSH 터널을 거치세요.': "By default this console listens only on the server itself (127.0.0.1). For remote use, go through an HTTPS reverse proxy or an SSH tunnel.",
  // server dashboard, tenancies
  '금지': "Denied", '서버와 모든 Tenancy의 상태': "Status of the server and all tenancies", 'Tenancy 및 서버': "Tenancies and server",
  'Tenancy {n} | 계정 {a} | 마지막 갱신 {t}': "Tenancies: {n} | Accounts: {a} | Last refreshed {t}", 'Unmigrated 포함': "Including Unmigrated",
  '승인 대기가 있는 Tenancy': "Tenancies with pending sign-ups", '옮겨 간 기기': "Devices moved", '평문 Tenancy': "Plaintext tenancies", '등록 기기': "Registered devices",
  '서버 한도': "Server quota", '한도 없음': "No quota", '정하지 않음': "Not set", '서버 주소': "Server domain",
  '서버 설정 > 외부 공개에서 정할 수 있습니다': "Set it in Server settings > Public access", 'Tenancy별 예외는 Tenancy 화면에서 정합니다': "Per-tenancy exceptions are set on the Tenancy page",
  '관리': "Manage", '구형·기존 사용자': "Legacy & existing users", '첨부 보관': "Stored files", '들어가기': "Open", 'Tenancy가 없습니다': "No tenancies",
  '서버 설정을 따름': "Follow server setting", '예외로 허용': "Allow (exception)", '예외로 금지': "Deny (exception)",
  '저장 공간 (MB, 0 = 무제한)': "Storage (MB, 0 = unlimited)", '파일 크기 상한 (MB, 0 = 없음)': "Max file size (MB, 0 = none)", '본문 상한 (KB)': "Max body size (KB)",
  '사용자 수 (0 = 무제한)': "Users (0 = unlimited)", '감사 사본에 첨부파일 보관 허용': "Allow keeping attachments in audit copies",
  '서버 교환 정책의 예외 (비워 두면 서버 설정을 따름)': "Exceptions to the server exchange policy (blank follows the server setting)",
  '조직마다 Tenancy 하나. 사용자는 클라이언트에서 서버 주소와 Tenancy ID를 입력합니다': "One tenancy per organization. Users enter the server address and Tenancy ID in the client",
  '새 Tenancy…': "New tenancy…", '새 Tenancy': "New tenancy", '행을 누르면 속성을 엽니다': "Click a row to open its properties", '예: example': "e.g. example",
  '예: Example Inc.': "e.g. Example Inc.", '비우면 나중에 추가': "Blank: add later",
  '2~32자의 영문 소문자, 숫자, -. 사용자가 클라이언트에 입력하고 주소에도 쓰이므로 나중에 바꿀 수 없습니다': "2–32 lowercase letters, digits or -. Users type it in the client and it is part of addresses, so it cannot be changed later",
  '첫 Tenancy 관리자 (선택)': "First tenancy administrator (optional)", 'Tenancy를 만들었습니다': "Tenancy created", '계정 / 기기': "Accounts / devices",
  '{a}명 · {d}대 · 접속 {o}': "{a} users · {d} devices · {o} online", '{auth} · 가입 {reg} · {enc}': "{auth} · sign-up {reg} · {enc}", '정보': "Details",
  '자원 한도와 교환 예외': "Resource limits and exchange exceptions",
  'Tenancy를 지우려면 먼저 모든 사용자가 없어야 합니다. "사용자 일괄 삭제"로 계정, 기기, 보관 쪽지를 한 번에 지울 수 있습니다.': "A tenancy can be deleted only when it has no users. \"Delete all users\" removes accounts, devices and stored messages in one go.",
  '사용자 일괄 삭제 ({n}명)': "Delete all users ({n})", '이 Tenancy 관리하기': "Manage this tenancy",
  '{name} Tenancy의 계정 {n}개와 등록된 기기, 받지 못한 쪽지가 모두 삭제됩니다. 되돌릴 수 없습니다.': "All {n} accounts of the {name} tenancy, their devices and undelivered messages are deleted. This cannot be undone.",
  '확인을 위해 Tenancy ID "{id}"를 입력하세요': "Type the Tenancy ID \"{id}\" to confirm", '모두 삭제': "Delete all", '계정 {n}개를 삭제했습니다': "Deleted {n} accounts",
  '{name} ({id}) Tenancy를 삭제할까요?': "Delete the tenancy {name} ({id})?",
  'Tenancy 관리자, 부서, 감사 로그, 감사 사본이 함께 삭제됩니다. 되돌릴 수 없습니다.': "Its administrators, departments, audit log and audit copies are deleted too. This cannot be undone.",
  // departments
  '사용자는 부서를 여러 개 가질 수 있습니다. 주소 "사용자@부서:tenancy:서버"와 클라이언트의 부서 선택에 쓰입니다': "Users can belong to several departments. Used in \"user@department:tenancy:server\" addresses and the client's department picker",
  '새 부서…': "New department…", '새 부서': "New department", '부서 ID': "Department ID", '구성원': "Members", '수정': "Edit", '부서가 없습니다': "No departments",
  '예: developers': "e.g. developers", '예: 개발팀': "e.g. Development", '1~32자의 영문 소문자, 숫자, . _ -. 주소에 쓰입니다': "1–32 lowercase letters, digits, . _ -. Used in addresses",
  '. 바꾸면 이 부서를 쓰던 주소가 더는 맞지 않습니다': ". Changing it breaks addresses that use this department", '사용자 이름, 쉼표로 여러 명': "Usernames, separated by commas",
  '구성원이 없습니다': "No members", '{name} 구성원': "{name} members", '아래 목록에서 체크한 사람은 부서에서 뺍니다': "People checked below are removed from the department",
  '적용': "Apply", '사용자를 찾을 수 없습니다: {n}': "User not found: {n}", '구성원을 바꿨습니다': "Members updated",
  '{name} ({id}) 부서를 삭제할까요?': "Delete the department {name} ({id})?",
  '구성원 {n}명의 계정은 그대로 두고 부서 소속만 지웁니다.': "The {n} members keep their accounts; only their membership is removed.",
  // API keys
  '스크립트가 사용자 대신 쪽지를 보내고 받는 Bearer 토큰. 사용자는 포털에서 직접 발급할 수 있습니다': "Bearer tokens that let scripts send and receive messages as a user. Users can issue their own in the portal",
  '키 발급…': "Issue key…", '키 발급': "Issue key", '이 Tenancy는 API 키를 허용하지 않아 발급된 키도 동작하지 않습니다. ': "This tenancy does not allow API keys, so issued keys do not work. ",
  '정책 > API 키': "Policy > API keys", '에서 켤 수 있습니다.': " can turn them on.", ' | 허용 안 됨': " | not allowed", '권한': "Scope", '보내기': "Send", '받기': "Receive",
  '마지막 사용': "Last used", '발급': "Issued", '키 ID': "Key ID", '폐기': "Revoke", '발급된 키가 없습니다': "No keys issued", '사용자 이름 또는 계정 ID': "Username or account ID",
  '예: 빌드 알림': "e.g. Build alerts", '받기 — 이 계정 앞으로 온 쪽지를 스크립트가 받아 확인(ack)합니다': "Receive — the script receives and acknowledges messages sent to this account",
  '계정을 찾을 수 없습니다': "Account not found", '{acc}의 "{name}" 키를 폐기할까요?': "Revoke the key \"{name}\" of {acc}?",
  '이 키가 받지 않은 쪽지 {n}건도 함께 지웁니다.': "Its {n} unreceived messages are removed too.", '폐기했습니다': "Revoked",
  '이 키는 지금 한 번만 보여 줍니다. 안전한 곳에 보관하세요.': "This key is shown only once. Keep it somewhere safe.",
  '사용 예: curl -H "Authorization: Bearer <키>" https://<릴레이 주소>/api/v1/me': "Example: curl -H \"Authorization: Bearer <key>\" https://<relay address>/api/v1/me",
  '복사하고 닫기': "Copy and close",
};

authScreen();
