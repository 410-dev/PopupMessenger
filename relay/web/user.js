'use strict';
// PopupMessenger relay: self-service page for account holders (relay port).
// Same conventions as the admin console: h() builds DOM nodes from text only, and
// UI text is Korean source strings passed through t() (English in EN, end of file).

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

const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
};

// ---------------------------------------------------------------- i18n
// See app.js: Korean keys, EN dictionary, "{name}" placeholders.
const LANG_KEY = 'relay-lang';
let LANG = (() => {
  const s = store.get(LANG_KEY);
  if (s === 'ko' || s === 'en') return s;
  return /^ko/i.test(navigator.language || '') ? 'ko' : 'en';
})();
const MISSING = new Set();
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
document.documentElement.lang = LANG;
const langPick = () => h('select', { class: 'langpick', title: 'Language / 언어', onchange: (e) => { LANG = e.target.value; store.set(LANG_KEY, LANG); document.documentElement.lang = LANG; start(); } },
  h('option', { value: 'ko', selected: LANG === 'ko' }, '한국어'),
  h('option', { value: 'en', selected: LANG === 'en' }, 'English'));

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
  b.addEventListener('click', () => { THEME = { auto: 'light', light: 'dark', dark: 'auto' }[THEME] || 'auto'; store.set(THEME_KEY, THEME); applyTheme(); draw(); });
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
function ago(x) {
  if (!x) return '—';
  const s = Math.floor((Date.now() - x) / 1000);
  if (s < 60) return t('방금');
  if (s < 3600) return t('{n}분 전', { n: Math.floor(s / 60) });
  if (s < 86400) return t('{n}시간 전', { n: Math.floor(s / 3600) });
  return t('{n}일 전', { n: Math.floor(s / 86400) });
}
function bytes(n) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(1)} ${u[i]}` : `${n} B`;
}
const num = (n) => (Number(n) || 0).toLocaleString(LANG === 'ko' ? 'ko-KR' : 'en-US');

async function api(method, path, body) {
  const opt = { method, credentials: 'same-origin', headers: { 'X-Relay-User': '1' } };
  if (body !== undefined) {
    opt.headers['Content-Type'] = 'application/json';
    opt.body = JSON.stringify(body);
  }
  const r = await fetch(path, opt);
  let j = {};
  try { j = await r.json(); } catch { /* empty */ }
  if (r.status === 401 && path !== '/api/me/login') { start(); throw new Error(j.error || t('로그인이 필요합니다')); }
  if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
  return j;
}

// ---------------------------------------------------------------- toast / dialog / pieces
const toasts = h('div', { class: 'toasts' });
document.body.append(toasts);
function toast(msg, err) {
  const el = h('div', { class: 'toast' + (err ? ' er' : '') }, msg);
  toasts.append(el);
  setTimeout(() => el.remove(), err ? 6000 : 3000);
}
const fail = (e) => toast(String(e.message || e), true);
// A confirmation dialog; resolves true on OK.
function confirmBox(title, lines, ok, danger) {
  return new Promise((resolve) => {
    const close = (v) => { scrim.remove(); box.remove(); document.removeEventListener('keydown', key); resolve(v); };
    const key = (e) => { if (e.key === 'Escape') close(false); if (e.key === 'Enter') close(true); };
    const okBtn = h('button', { class: 'btn ' + (danger ? 'dan solid' : 'pri'), onclick: () => close(true) }, ok);
    const scrim = h('div', { class: 'scrim', onclick: () => close(false) });
    const box = h('div', { class: 'modal', role: 'dialog' },
      h('div', { class: 'hd' }, h('span', {}, title), h('span', { class: 'sp' }), h('button', { class: 'x', title: t('닫기'), onclick: () => close(false) }, '✕')),
      h('div', { class: 'bd' }, h('div', { class: 'lines' }, lines.filter(Boolean).map((l) => h('div', {}, l)))),
      h('div', { class: 'ft' }, okBtn, h('button', { class: 'btn', onclick: () => close(false) }, t('취소'))));
    document.addEventListener('keydown', key);
    document.body.append(scrim, box);
    okBtn.focus();
  });
}
const field = (label, input) => h('div', { class: 'fld' }, h('label', {}, label), input);
const chip = (cls, text) => h('span', { class: 'chip ' + cls }, text);
const props = (pairs) => h('div', { class: 'props' }, pairs.filter(Boolean).map(([k, v]) => h('div', { class: 'pr' }, h('div', { class: 'k' }, k), h('div', { class: 'v' }, v))));
function sect(title, opt = {}, ...kids) {
  const sub = h('span', { class: 'sec-s' }, opt.sub || '');
  const el = h('section', { class: 'sec' }, h('div', { class: 'sec-hd' }, h('span', { class: 'sec-t' }, title), sub, h('span', { class: 'sp' }), opt.right), kids);
  el.sub = (...x) => sub.replaceChildren(...x);
  return el;
}
// A plain table: cols [header, cls]; rows of cells.
function table(cols, rows, empty) {
  if (!rows.length) return h('div', { class: 'empty' }, empty);
  return h('div', { class: 'tw' }, h('table', { class: 'g' },
    h('thead', {}, h('tr', {}, cols.map(([x, cls]) => h('th', { class: cls || '' }, x)))),
    h('tbody', {}, rows.map((cells) => h('tr', {}, cells.map((c, i) => h('td', { class: cols[i][1] || '' }, c)))))));
}

// Korean source strings; translated where shown.
const ACTIONS = {
  register: '가입', device_add: '기기 등록', device_move: '기기를 이 계정으로 이동', login_locked: '로그인 잠김',
  user_login: '웹 로그인', user_login_failed: '웹 로그인 실패', user_password: '비밀번호 변경@@ev', user_name: '이름 변경',
  user_device_kick: '기기 연결 끊기', user_device_delete: '기기 등록 해제',
  api_key_create: 'API 키 발급', api_key_revoke: 'API 키 폐기@@ev', relay_receive: '릴레이 수신 설정',
  account_moved_in: '다른 Tenancy에서 옮겨 옴',
};
const TEN_KEY = 'relay-portal-tenancy';
const ENC = { plain: '평문', tls: '서버 통신 암호화 (TLS)', e2ee: '종단간 암호화' };

let S = null; // session info

// ---------------------------------------------------------------- sign-in
async function start() {
  S = await fetch('/api/me/session', { credentials: 'same-origin' }).then((r) => r.json());
  document.title = t('{server} · 내 계정', { server: S.server_name });
  if (S.user) return home();
  const saved = store.get(TEN_KEY) || '';
  const tenancy = h('input', { type: 'text', autocomplete: 'organization', value: saved, placeholder: t('예: example') });
  const user = h('input', { type: 'text', autocomplete: 'username' });
  const pass = h('input', { type: 'password', autocomplete: 'current-password' });
  const err = h('div', { class: 'err' });
  const btn = h('button', { class: 'btn pri' }, t('로그인'));
  async function go(e) {
    e.preventDefault();
    err.textContent = '';
    btn.disabled = true;
    const tn = tenancy.value.trim().toLowerCase();
    try {
      await api('POST', '/api/me/login', { tenancy: tn, username: user.value.trim(), password: pass.value });
      store.set(TEN_KEY, tn);
      await start();
    } catch (x) { err.textContent = String(x.message || x); btn.disabled = false; }
  }
  $app.replaceChildren(h('div', { class: 'login' },
    h('div', { class: 'login-top' }, h('span', { class: 'brand' }, S.server_name), h('span', { class: 'sp' }), langPick(), themeBtn()),
    h('div', { class: 'login-mid' }, h('form', { class: 'dlg', onsubmit: go },
      h('div', { class: 'dlg-hd' }, t('내 계정 관리')),
      h('div', { class: 'dlg-bd' },
        h('div', { class: 'dlg-title' }, h('div', { class: 'mu' }, t('PopupMessenger 릴레이')), h('div', { class: 'big' }, S.server_name)),
        !S.secure && h('div', { class: 'warnbox' }, t('암호화되지 않은 연결입니다. 평문을 허용하지 않는 Tenancy에는 로그인할 수 없습니다. 가능하면 https:// 주소로 다시 접속하세요.')),
        h('div', { class: 'form2' },
          field('Tenancy ID', tenancy),
          field(t('사용자 이름'), user), field(t('비밀번호'), pass)),
        err,
        h('div', { class: 'help' }, t('PopupMessenger 앱에서 릴레이 서버를 연결할 때 쓴 Tenancy ID, 사용자 이름, 비밀번호로 로그인합니다. 앱에 Tenancy ID 입력란이 없었다면 비워 두세요.'))),
      h('div', { class: 'dlg-ft' }, btn)))));
  (saved ? user : tenancy).focus();
}

// ---------------------------------------------------------------- portal
async function home() {
  const segs = [h('span', { class: 'c0' }, S.server_name), S.tenancy && h('span', {}, t('Tenancy: {name}', { name: S.tenancy.name }), h('span', { class: 'cid' }, ` (${S.tenancy.id})`)), h('span', { class: 'cur' }, t('내 계정'))].filter(Boolean);
  const top = h('header', { class: 'top' },
    h('div', { class: 'crumbs' }, segs.flatMap((s, i) => (i ? [h('span', { class: 'sep' }, '▸'), s] : [s]))),
    h('span', { class: 'sp' }), langPick(), themeBtn(),
    h('span', { class: 'who' }, S.user),
    h('button', { class: 'tbtn', type: 'button', onclick: async () => { try { await api('POST', '/api/me/logout'); } catch { /* */ } start(); } }, t('로그아웃')));
  const body = h('main', { class: 'portal' });
  $app.replaceChildren(h('div', { class: 'pshell' }, top, body));
  await draw(body);
}

async function draw(body) {
  const d = await api('GET', '/api/me');
  const a = d.account, p = d.policy;
  const name = h('input', { type: 'text', value: a.name });
  const saveName = async () => {
    try { await api('PATCH', '/api/me', { name: name.value }); toast(t('이름을 바꿨습니다')); } catch (e) { fail(e); }
  };
  const online = d.devices.filter((x) => x.online).length;
  body.replaceChildren(
    sect(t('계정'), { sub: a.username || '' }, props([
      [t('사용자 이름'), h('b', {}, a.username || '—')],
      [t('표시 이름'), h('div', { class: 'inl' }, name, h('button', { class: 'btn sm', onclick: saveName }, t('저장')))],
      [t('계정 ID'), h('span', { class: 'mono' }, a.id)],
      [t('가입'), ts(a.created)],
      [t('마지막 접속'), a.last_seen ? `${ts(a.last_seen)} (${ago(a.last_seen)})` : '—'],
      [t('기기'), t('{n} / {max}대 등록 · {o}대 접속 중', { n: d.devices.length, max: a.max_devices, o: online })],
      [t('받을 쪽지'), t('{n}건 (기기 단위)', { n: num(a.pending) })],
      [t('보낸 첨부 보관'), `${bytes(a.stored_bytes)}${a.quota_mb ? ` / ${num(a.quota_mb)} MB` : ''}`],
      [t('서버 정책'), t('{enc} · 첨부 {files} · 보관 {h}시간', {
        enc: ENC[p.encryption] ? t(ENC[p.encryption]) : p.encryption,
        files: p.relay_max_files ? t('{n}개, 파일당 {s}', { n: p.relay_max_files, s: bytes(p.relay_max_file_size) }) : t('중계 안 함'),
        h: p.retention_hours.text,
      })],
    ])),
    devicesPanel(d.devices, () => draw(body)),
    await keysPanel(() => draw(body)),
    passwordPanel(p.min_password_len),
    activityPanel());
}

// API keys: bearer tokens for scripts, acting as this account.
async function keysPanel(redraw) {
  const r = await api('GET', '/api/me/keys');
  if (!r.allowed && !r.items.length) return null;
  const name = h('input', { type: 'text', placeholder: t('예: 빌드 알림') });
  const send = h('input', { type: 'checkbox', checked: true });
  const recv = h('input', { type: 'checkbox' });
  const shown = h('div', { class: 'tokenbox' });
  async function create() {
    try {
      const k = await api('POST', '/api/me/keys', { name: name.value.trim(), send: send.checked, receive: recv.checked });
      const tk = h('input', { type: 'text', readonly: true, value: k.token, class: 'mono', onfocus: (e) => e.target.select() });
      tk.style.width = '100%';
      shown.replaceChildren(h('div', { class: 'warnbox' }, t('새 API 키입니다. 지금 한 번만 보여 줍니다. 안전한 곳에 보관하세요.')),
        h('div', { class: 'inl' }, tk, h('button', { class: 'btn sm', onclick: () => navigator.clipboard.writeText(k.token).then(() => toast(t('복사했습니다'))) }, t('복사'))));
      name.value = '';
      list.replaceChildren(keyTable((await api('GET', '/api/me/keys')).items));
    } catch (e) { fail(e); }
  }
  async function revoke(k) {
    if (!(await confirmBox(t('API 키 폐기'), [t('"{name}" 키를 폐기할까요?', { name: k.name }), t('이 키를 쓰는 스크립트는 더는 동작하지 않습니다.')], t('폐기'), true))) return;
    try { await api('DELETE', `/api/me/keys/${k.id}`); toast(t('폐기했습니다')); redraw(); } catch (e) { fail(e); }
  }
  const keyTable = (items) => table(
    [[t('이름')], [t('권한')], [t('받을 쪽지'), 'num'], [t('마지막 사용')], [t('발급@@col')], ['', 'act']],
    items.map((k) => [
      k.name,
      [k.send && chip('ac', t('보내기')), ' ', k.receive && chip('ok', t('받기'))],
      k.receive ? num(k.pending) : '',
      k.last_used ? `${ts(k.last_used)} (${k.last_ip})` : '—',
      ts(k.created),
      h('button', { class: 'btn sm dan', onclick: () => revoke(k) }, t('폐기')),
    ]),
    t('발급한 키가 없습니다'));
  const list = h('div', {}, keyTable(r.items));
  return sect(t('API 키'), { sub: t('{n}개', { n: r.items.length }) },
    h('div', { class: 'secnote' }, t('터미널 스크립트 등에서 이 계정으로 쪽지를 보내거나 받을 때 씁니다: '),
      h('span', { class: 'mono' }, 'curl -H "Authorization: Bearer <key>" …/api/v1/send'),
      t('. 받기 권한 키는 이 계정 앞으로 온 쪽지를 따로 받아 확인(ack)합니다.')),
    !r.allowed && h('div', { class: 'secnote' }, h('div', { class: 'warnbox' }, t('관리자가 API 키를 꺼 두어 지금은 키가 동작하지 않습니다.'))),
    r.allowed && h('div', { class: 'tb' }, h('span', { class: 'tl' }, t('이름')), name,
      h('label', { class: 'line' }, send, t('보내기')), h('label', { class: 'line' }, recv, t('받기')),
      h('button', { class: 'btn pri', onclick: create }, t('발급'))),
    shown, list);
}

function devicesPanel(devs, redraw) {
  async function act(dv, what) {
    const nm = dv.name || dv.id;
    const lines = what === 'kick'
      ? [t('{name} 기기의 연결을 끊을까요?', { name: nm }), t('기기는 곧 다시 연결합니다. 막으려면 등록을 해제하세요.')]
      : [t('{name} 기기의 등록을 해제할까요?', { name: nm }), t('이 기기는 다시 비밀번호로 로그인해야 하며, 이 기기가 아직 받지 못한 쪽지는 삭제됩니다. 기기를 잃어버렸을 때 쓰세요.')];
    if (!(await confirmBox(what === 'kick' ? t('연결 끊기') : t('등록 해제'), lines, what === 'kick' ? t('연결 끊기') : t('등록 해제'), true))) return;
    try {
      await api(what === 'kick' ? 'POST' : 'DELETE', `/api/me/devices/${encodeURIComponent(dv.id)}${what === 'kick' ? '/kick' : ''}`);
      toast(what === 'kick' ? t('연결을 끊었습니다') : t('등록을 해제했습니다'));
      redraw();
    } catch (e) { fail(e); }
  }
  return sect(t('등록된 기기'), { sub: t('{n}대', { n: devs.length }) }, table(
    [[t('기기@@one')], [t('상태')], [t('기기 ID'), 'mono'], [t('호스트')], [t('앱'), 'mono'], [t('마지막 IP'), 'mono'], [t('마지막 접속')], [t('받을 쪽지'), 'num'], [t('키 지문'), 'mono'], ['', 'act']],
    devs.map((dv) => [
      [h('span', { class: 'dot' + (dv.online ? ' ok' : '') }), ' ', dv.name || h('span', { class: 'mu' }, t('(이름 없음)'))],
      dv.disabled ? chip('er', t('정지됨')) : dv.online ? chip('ok', t('접속 중')) : chip('ne', t('오프라인')),
      dv.id, dv.host || '', dv.app || '', dv.last_ip || '',
      dv.online ? t('지금') : ago(dv.last_seen),
      dv.pending ? num(dv.pending) : '',
      h('span', { title: t('종단간 암호화 키 지문') }, dv.key || ''),
      [dv.online && h('button', { class: 'btn sm', onclick: () => act(dv, 'kick') }, t('연결 끊기')), ' ',
        h('button', { class: 'btn sm dan', onclick: () => act(dv, 'delete') }, t('등록 해제'))],
    ]),
    t('등록된 기기가 없습니다')));
}

function passwordPanel(min) {
  const old = h('input', { type: 'password', autocomplete: 'current-password' });
  const n1 = h('input', { type: 'password', autocomplete: 'new-password' });
  const n2 = h('input', { type: 'password', autocomplete: 'new-password' });
  async function change() {
    if (n1.value !== n2.value) return fail(t('새 비밀번호가 서로 다릅니다'));
    try {
      await api('POST', '/api/me/password', { old: old.value, new: n1.value });
      old.value = n1.value = n2.value = '';
      toast(t('비밀번호를 바꿨습니다. 다른 곳의 웹 로그인은 끝났습니다'));
    } catch (e) { fail(e); }
  }
  return sect(t('비밀번호 변경'), {},
    h('div', { class: 'pw' },
      field(t('현재 비밀번호'), old), field(t('새 비밀번호 ({n}자 이상)', { n: min }), n1), field(t('새 비밀번호 확인'), n2),
      h('div', {}, h('button', { class: 'btn pri', onclick: change }, t('변경')))),
    h('div', { class: 'foot' }, t('이미 등록된 기기는 비밀번호를 바꿔도 로그아웃되지 않습니다. 기기를 막으려면 위에서 등록을 해제하세요.')));
}

function activityPanel() {
  const box = h('div');
  const pager = h('div', { class: 'pager' });
  let page = 1;
  const per = 50;
  const sec = sect(t('내 활동 기록'), { right: pager },
    h('div', { class: 'secnote' }, t('가입, 기기 등록·이동·해제, 웹 로그인, 비밀번호 변경 등 이 계정과 기기에서 일어난 일입니다. 관리자가 한 작업은 표시되지 않습니다.')), box);
  async function load() {
    try {
      const r = await api('GET', `/api/me/activity?page=${page}&per=${per}`);
      box.replaceChildren(table(
        [[t('시각'), 'mono'], [t('작업')], [t('주체'), 'mono'], [t('내용'), 'wrap']],
        r.items.map((x) => [ts(x.ts), ACTIONS[x.action] ? t(ACTIONS[x.action]) : x.action, x.actor, x.detail]),
        t('기록이 없습니다')));
      const pages = Math.max(1, Math.ceil(r.total / per));
      sec.sub(t('{n}건', { n: num(r.total) }));
      pager.replaceChildren(
        h('span', { class: 'mu' }, t('{page}/{pages}쪽', { page: num(page), pages: num(pages) })),
        h('button', { class: 'pb', disabled: page <= 1, title: t('이전'), onclick: () => { page--; load(); } }, '‹'),
        h('button', { class: 'pb', disabled: page >= pages, title: t('다음'), onclick: () => { page++; load(); } }, '›'));
    } catch (e) { fail(e); }
  }
  load();
  return sec;
}

// ---------------------------------------------------------------- English dictionary
const EN = {
  '테마: 시스템 설정': "Theme: system", '테마: 밝게': "Theme: light", '테마: 어둡게': "Theme: dark",
  '방금': "just now", '{n}분 전': "{n} min ago", '{n}시간 전': "{n} h ago", '{n}일 전': "{n} d ago",
  '로그인이 필요합니다': "Please sign in", '닫기': "Close", '취소': "Cancel", '복사했습니다': "Copied", '복사': "Copy",
  // activity actions
  '가입': "Signed up", '기기 등록': "Device registered", '기기를 이 계정으로 이동': "Device moved to this account", '로그인 잠김': "Sign-in locked",
  '웹 로그인': "Web sign-in", '웹 로그인 실패': "Web sign-in failed", '비밀번호 변경': "Change password", '비밀번호 변경@@ev': "Password changed", '이름 변경': "Name changed",
  '기기 연결 끊기': "Device disconnected", '기기 등록 해제': "Device unregistered", 'API 키 발급': "API key issued", 'API 키 폐기': "Revoke API key", 'API 키 폐기@@ev': "API key revoked",
  '발급@@col': "Issued",
  '릴레이 수신 설정': "Relay receive setting", '다른 Tenancy에서 옮겨 옴': "Moved in from another tenancy",
  '평문': "Plaintext", '서버 통신 암호화 (TLS)': "Transport encryption (TLS)", '종단간 암호화': "End-to-end encryption",
  // sign-in
  '{server} · 내 계정': "{server} · My account", '예: example': "e.g. example", '로그인': "Sign in", '내 계정 관리': "Manage my account",
  'PopupMessenger 릴레이': "PopupMessenger relay",
  '암호화되지 않은 연결입니다. 평문을 허용하지 않는 Tenancy에는 로그인할 수 없습니다. 가능하면 https:// 주소로 다시 접속하세요.': "This connection is not encrypted. You cannot sign in to tenancies that do not allow plaintext. If possible, reconnect with an https:// address.",
  '사용자 이름': "Username", '비밀번호': "Password",
  'PopupMessenger 앱에서 릴레이 서버를 연결할 때 쓴 Tenancy ID, 사용자 이름, 비밀번호로 로그인합니다. 앱에 Tenancy ID 입력란이 없었다면 비워 두세요.': "Sign in with the Tenancy ID, username and password you used to connect the PopupMessenger app to the relay. If the app had no Tenancy ID field, leave it blank.",
  // account
  'Tenancy: {name}': "Tenancy: {name}", '내 계정': "My account", '로그아웃': "Sign out", '이름을 바꿨습니다': "Name changed", '계정': "Account",
  '표시 이름': "Display name", '저장': "Save", '계정 ID': "Account ID", '마지막 접속': "Last seen", '기기': "Devices",
  '{n} / {max}대 등록 · {o}대 접속 중': "{n} / {max} registered · {o} online", '받을 쪽지': "Pending messages", '{n}건 (기기 단위)': "{n} (per device)",
  '보낸 첨부 보관': "Stored uploads", '서버 정책': "Server policy", '{enc} · 첨부 {files} · 보관 {h}시간': "{enc} · attachments {files} · kept {h} h",
  '{n}개, 파일당 {s}': "{n}, {s} per file", '중계 안 함': "not relayed",
  // API keys
  '예: 빌드 알림': "e.g. Build alerts", '새 API 키입니다. 지금 한 번만 보여 줍니다. 안전한 곳에 보관하세요.': "Your new API key. It is shown only once. Keep it somewhere safe.",
  '"{name}" 키를 폐기할까요?': "Revoke the key \"{name}\"?", '이 키를 쓰는 스크립트는 더는 동작하지 않습니다.': "Scripts using this key stop working.",
  '폐기': "Revoke", '폐기했습니다': "Revoked", '이름': "Name", '권한': "Scope", '마지막 사용': "Last used", '발급': "Issue", '보내기': "Send", '받기': "Receive",
  '발급한 키가 없습니다': "You have no keys", 'API 키': "API keys", '{n}개': "{n}",
  '터미널 스크립트 등에서 이 계정으로 쪽지를 보내거나 받을 때 씁니다: ': "Used by terminal scripts and the like to send or receive messages as this account: ",
  '. 받기 권한 키는 이 계정 앞으로 온 쪽지를 따로 받아 확인(ack)합니다.': ". Keys with the receive scope get their own copy of messages sent to this account and acknowledge them.",
  '관리자가 API 키를 꺼 두어 지금은 키가 동작하지 않습니다.': "An administrator has turned API keys off, so keys do not work right now.",
  // devices
  '{name} 기기의 연결을 끊을까요?': "Disconnect the device {name}?", '기기는 곧 다시 연결합니다. 막으려면 등록을 해제하세요.': "The device reconnects soon. Unregister it to keep it out.",
  '{name} 기기의 등록을 해제할까요?': "Unregister the device {name}?",
  '이 기기는 다시 비밀번호로 로그인해야 하며, 이 기기가 아직 받지 못한 쪽지는 삭제됩니다. 기기를 잃어버렸을 때 쓰세요.': "The device must sign in with the password again, and messages it has not received yet are deleted. Use this when a device is lost.",
  '연결 끊기': "Disconnect", '등록 해제': "Unregister", '연결을 끊었습니다': "Disconnected", '등록을 해제했습니다': "Unregistered",
  '등록된 기기': "Registered devices", '기기@@one': "Device", '{n}대': "{n} devices", '상태': "Status", '기기 ID': "Device ID", '호스트': "Host", '앱': "App", '마지막 IP': "Last IP",
  '키 지문': "Key fingerprint", '(이름 없음)': "(no name)", '정지됨': "Disabled", '접속 중': "Online", '오프라인': "Offline", '지금': "now",
  '종단간 암호화 키 지문': "End-to-end encryption key fingerprint", '등록된 기기가 없습니다': "No registered devices",
  // password
  '새 비밀번호가 서로 다릅니다': "The new passwords do not match", '비밀번호를 바꿨습니다. 다른 곳의 웹 로그인은 끝났습니다': "Password changed. Web sessions elsewhere were signed out",
  '현재 비밀번호': "Current password", '새 비밀번호 ({n}자 이상)': "New password ({n}+ characters)", '새 비밀번호 확인': "Confirm new password", '변경': "Change",
  '이미 등록된 기기는 비밀번호를 바꿔도 로그아웃되지 않습니다. 기기를 막으려면 위에서 등록을 해제하세요.': "Registered devices stay signed in when the password changes. Unregister a device above to keep it out.",
  // activity
  '내 활동 기록': "My activity",
  '가입, 기기 등록·이동·해제, 웹 로그인, 비밀번호 변경 등 이 계정과 기기에서 일어난 일입니다. 관리자가 한 작업은 표시되지 않습니다.': "What happened on this account and its devices: sign-up, device registration, moves and removal, web sign-ins, password changes. Actions by administrators are not shown.",
  '시각': "Time", '작업': "Action", '주체': "Actor", '내용': "Detail", '기록이 없습니다': "No records", '{n}건': "{n} records", '{page}/{pages}쪽': "Page {page}/{pages}",
  '이전': "Previous", '다음': "Next",
};

start();
