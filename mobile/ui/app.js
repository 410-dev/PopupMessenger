'use strict';
// PopupMessenger mobile. One page: the setup screen until the phone has joined a
// relay server, then tabs (received, sent, people, settings) with sheets stacked on
// top (reading, writing, picking recipients). Sheets are history entries, so the
// Android back button closes them. The phone may have several relay connections
// (a server and a tenancy each); contacts and recipients carry their connection.

const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;
const $app = document.getElementById('app');
const $layers = document.getElementById('layers');

// Android draws edge to edge without CSS safe-area values; the native side reports
// the system bars and the keyboard here (mobile/plugin, NativePlugin.load).
window.__pmInsets = (top, bottom, left, right, ime) => {
  const r = document.documentElement.style;
  r.setProperty('--top', `${top}px`);
  r.setProperty('--left', `${left}px`);
  r.setProperty('--right', `${right}px`);
  r.setProperty('--bottom', `${ime > 0 ? 0 : bottom}px`);
  r.setProperty('--kb', `${ime}px`);
  document.activeElement?.scrollIntoView?.({ block: 'nearest' });
};

const S = {
  version: '', platform: '',
  settings: {}, relay: { linked: false, connections: [] },
  contacts: [], departments: [], inbox: [], sent: [],
  screen: '', tab: 'inbox',
  setup: { step: 1, addr: '', tenancy: '', probe: null, mode: 'login', err: '', busy: false },
  // Adding another connection from settings (the setup screen with a way back).
  adding: false,
  search: '',
  popups: [],
};

// Tiny DOM helper. Strings become text nodes, so message content is never parsed as HTML.
function h(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (k in e && (typeof v !== 'string' || k === 'value')) e[k] = v;
    else if (v != null && v !== false) e.setAttribute(k, v);
  }
  for (const c of kids.flat(Infinity)) if (c != null && c !== false) e.append(c instanceof Node ? c : String(c));
  return e;
}

/** Replaces an element's children, skipping null/false like h() does. */
function put(el, ...kids) {
  el.replaceChildren(...kids.flat(Infinity).filter((c) => c != null && c !== false));
}

// Icons (Feather, MIT). Static markup only.
const ICONS = {
  inbox: '<polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/>',
  send: '<line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>',
  users: '<path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  gear: '<line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="1" y1="14" x2="7" y2="14"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="17" y1="16" x2="23" y2="16"/>',
  pen: '<path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"/>',
  back: '<polyline points="15 18 9 12 15 6"/>',
  right: '<polyline points="9 18 15 12 9 6"/>',
  down: '<polyline points="6 9 12 15 18 9"/>',
  refresh: '<polyline points="23 4 23 10 17 10"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/>',
  clip: '<path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/>',
  check: '<polyline points="20 6 9 17 4 12"/>',
  trash: '<polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  reply: '<polyline points="9 14 4 9 9 4"/><path d="M20 20v-7a4 4 0 0 0-4-4H4"/>',
  x: '<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>',
};
function ic(name, px = 22) {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('width', px);
  s.setAttribute('height', px);
  s.setAttribute('fill', 'none');
  s.setAttribute('stroke', 'currentColor');
  s.setAttribute('stroke-width', '2');
  s.setAttribute('stroke-linecap', 'round');
  s.setAttribute('stroke-linejoin', 'round');
  s.innerHTML = ICONS[name];
  return s;
}
const iconBtn = (name, title, onclick, extra = {}) => h('button', { class: 'icon', title, 'aria-label': title, onclick, ...extra }, ic(name));
const textBtn = (label, onclick, extra = {}) => h('button', { class: 'icon text', onclick, ...extra }, label);

const size = (n) => {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(1)} ${u[i]}` : `${n} B`;
};
const time = (ts) => {
  const d = new Date(ts), now = new Date();
  const hm = d.toLocaleTimeString('ko-KR', { hour: '2-digit', minute: '2-digit' });
  if (d.toDateString() === now.toDateString()) return hm;
  const y = new Date(now); y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return `어제 ${hm}`;
  return d.getFullYear() === now.getFullYear() ? `${d.getMonth() + 1}/${d.getDate()}` : `${d.getFullYear()}. ${d.getMonth() + 1}. ${d.getDate()}.`;
};
const fullTime = (ts) => new Date(ts).toLocaleString('ko-KR', { dateStyle: 'medium', timeStyle: 'short' });
const initial = (name) => (name || '?').trim().charAt(0).toUpperCase() || '?';
const fp = (s) => (s || '').match(/.{1,4}/g)?.join(' ') || '';
const err = (e) => String(e?.message || e || '오류');
const uid = () => Math.random().toString(36).slice(2, 10);

let toastTimer = 0;
function toast(text) {
  const t = document.getElementById('toast');
  t.textContent = text;
  t.classList.add('on');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('on'), 2600);
}

// Reachability (contacts' "state"): filled green = connected, green ring = away but
// the relay keeps messages for it, red = do not disturb, gray = cannot receive.
const STATE = {
  online: '접속 중',
  hold: '앱이 꺼져 있음 · 릴레이 서버가 보관했다가 전달',
  dnd: '방해금지 · 알림 없이 받음',
  off: '받을 수 없음',
};
const dot = (state) => h('span', { class: `dot s-${state || 'off'}`, title: STATE[state] || STATE.off });
const avatar = (name, state) => h('div', { class: 'avatar' }, initial(name), state && dot(state));
const handle = (u) => u && h('span', { class: 'handle' }, `@${u}`);

// Connections. Messages and recipients saved before there were several name none:
// they belong to the first.
const conns = () => S.relay.connections || [];
const connId = (id) => id || conns()[0]?.id || '';
const connOf = (id) => conns().find((c) => c.id === connId(id));
const multi = () => conns().length > 1;
/** Which connection something belongs to, shown only when there are several. */
const connTag = (id) => multi() && connOf(id) && h('span', { class: 'tag' }, connOf(id).label);
const contactOf = (conn, acc) => S.contacts.find((c) => c.conn === connId(conn) && c.account === acc);
/** The address of a sender who is not in the contacts (another tenancy). */
const strangerAddr = (m) => (m.from_address && !contactOf(m.conn, m.from) ? m.from_address : '');
/** Recipients are {conn, to, name}; this tells them apart. */
const rkey = (t) => `${connId(t.conn)}|${t.to}`;
const TENANCY_RE = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])$/;

// ---------------------------------------------------------------- sheets & history

const layers = [];
let popWaiters = [];

function pushLayer(el, opts = {}) {
  const L = { el, ...opts };
  layers.push(L);
  $layers.append(el);
  history.pushState({ depth: layers.length }, '');
  return L;
}

/** Closes the top sheet (through history, like the back button). */
function popLayer() {
  return new Promise((resolve) => {
    if (!layers.length) return resolve();
    popWaiters.push(resolve);
    history.back();
  });
}

window.addEventListener('popstate', () => {
  const L = layers.pop();
  if (L) {
    L.el.remove();
    L.onClose?.();
  }
  const w = popWaiters;
  popWaiters = [];
  w.forEach((f) => f());
});

async function closeAllLayers() {
  while (layers.length) await popLayer();
}

function refreshLayers() {
  for (const L of layers) L.refresh?.();
}

/** A yes/no question as a modal sheet (WKWebView has no confirm()). Back = no. */
function ask(text, ok = '확인', danger = false) {
  return new Promise((resolve) => {
    let answer = false;
    const choose = (v) => { answer = v; popLayer(); };
    const card = h('div', { class: 'popup' },
      h('div', { class: 'content', style: 'padding-top:18px' }, h('div', { class: 'body' }, text)),
      h('div', { class: 'btns' },
        h('button', { class: 'btn', onclick: () => choose(false) }, '취소'),
        h('button', { class: `btn ${danger ? 'danger' : 'primary'}`, onclick: () => choose(true) }, ok)));
    pushLayer(h('div', { class: 'layer modal', onclick: (e) => { if (e.target === e.currentTarget) choose(false); } }, card),
      { onClose: () => resolve(answer) });
  });
}

function sheet(title, { left, right, sub } = {}) {
  const bar = h('div', { class: 'bar' },
    left === undefined ? iconBtn('back', '뒤로', () => popLayer()) : left,
    h('h1', {}, title, sub && h('span', { class: 'sub' }, sub)),
    right);
  const el = h('div', { class: 'layer' }, bar);
  return el;
}

// ---------------------------------------------------------------- data

async function loadRelay() { S.relay = await invoke('relay_info'); }
async function loadContacts() {
  [S.contacts, S.departments] = S.relay.linked ? await Promise.all([invoke('contacts'), invoke('departments')]) : [[], []];
}
async function loadMessages() {
  const m = await invoke('messages');
  S.inbox = m.inbox;
  S.sent = m.sent;
}

// ---------------------------------------------------------------- top level

function route() {
  const r = S.relay;
  const want = !r.linked || S.adding ? 'setup' : (r.state === 'pending' || r.state === 'fatal') ? 'blocked' : 'main';
  if (want !== S.screen) {
    S.screen = want;
    if (want !== 'main') closeAllLayers();
  }
  if (want === 'setup') drawSetup();
  else if (want === 'blocked') drawBlocked();
  else drawMain();
}

// ---------------------------------------------------------------- setup (joining a relay)

function drawSetup() {
  const st = S.setup;
  const steps = h('div', { class: 'steps' }, [1, 2, 3].map((i) => h('i', { class: i <= st.step ? 'on' : '' })));
  const errEl = st.err && h('div', { class: 'err' }, st.err);
  let body;
  if (st.step === 1) {
    const addr = h('input', { type: 'url', inputmode: 'url', autocapitalize: 'off', autocorrect: 'off', spellcheck: false,
      placeholder: 'https://relay.example.com', value: st.addr, enterkeyhint: 'next',
      oninput: (e) => { st.addr = e.target.value; },
      onkeydown: (e) => { if (e.key === 'Enter') tenancy.focus(); } });
    const tenancy = h('input', { autocapitalize: 'off', autocorrect: 'off', spellcheck: false, autocomplete: 'off',
      placeholder: 'example', value: st.tenancy, enterkeyhint: 'next', maxlength: 32,
      oninput: (e) => { st.tenancy = e.target.value; },
      onkeydown: (e) => { if (e.key === 'Enter') next(); } });
    const next = async () => {
      const tn = st.tenancy.trim().toLowerCase();
      if (!st.addr.trim()) { st.err = '서버 주소를 입력하세요'; return drawSetup(); }
      if (!tn) { st.err = 'Tenancy ID를 입력하세요'; return drawSetup(); }
      if (!TENANCY_RE.test(tn)) { st.err = "Tenancy ID는 영문 소문자, 숫자, '-'로 된 2~32자입니다"; return drawSetup(); }
      st.tenancy = tn;
      st.busy = true; st.err = ''; drawSetup();
      try {
        st.probe = await invoke('probe', { url: st.addr.trim(), tenancy: tn });
        st.step = 2;
        const p = st.probe.policy || {};
        st.mode = p.auth === 'password' ? 'login' : 'device';
      } catch (e) { st.err = err(e); }
      st.busy = false;
      drawSetup();
    };
    const cancel = async () => {
      S.adding = false;
      S.setup = { step: 1, addr: '', tenancy: '', probe: null, mode: 'login', err: '', busy: false };
      route();
    };
    body = [
      S.adding
        ? h('div', { class: 'hero' }, h('h1', {}, '릴레이 연결 추가'),
          h('div', { class: 'muted' }, '다른 서버나 같은 서버의 다른 Tenancy(조직)에 이 휴대폰을 함께 연결합니다.'))
        : h('div', { class: 'hero' },
          h('img', { src: 'icon.png', alt: '' }),
          h('h1', {}, 'PopupMessenger'),
          h('div', { class: 'muted' }, '모바일 앱은 릴레이 서버를 통해서만 쪽지를 주고받습니다. 먼저 사용할 릴레이 서버에 연결하세요.')),
      h('div', { class: 'card' },
        h('label', { class: 'f' }, '릴레이 서버 주소', addr),
        h('div', { class: 'muted' }, '예: https://relay.example.com, relay.example.com:45443'),
        h('label', { class: 'f' }, 'Tenancy ID', tenancy),
        h('div', { class: 'muted' }, '관리자에게 받은 조직 ID입니다 (영문 소문자, 숫자, -).'),
        errEl,
        h('button', { class: 'btn primary', disabled: st.busy, onclick: next }, st.busy ? '확인하는 중…' : '다음')),
      S.adding && h('button', { class: 'btn', onclick: cancel }, '취소'),
    ];
    requestAnimationFrame(() => {
      if (st.busy) return;
      if (!st.addr) addr.focus();
      else if (!st.tenancy) tenancy.focus();
    });
  } else if (st.step === 2) {
    const p = st.probe;
    const pol = p.policy || {};
    const trust = !p.tls
      ? h('div', { class: 'warn' }, '⚠ 암호화되지 않은 연결입니다. 신뢰할 수 있는 네트워크에서만 사용하세요.')
      : p.trusted
        ? h('div', { style: 'color:var(--ok);font-size:13px' }, '✓ 인증 기관이 확인한 인증서입니다')
        : h('div', { class: 'warn' }, '⚠ 자체 서명 인증서입니다. 아래 지문이 관리자에게 받은 지문과 같은지 확인하세요. 이후에는 이 지문으로만 연결합니다.');
    const enc = { plain: '없음', tls: '통신 구간 (TLS)', e2ee: '종단간 암호화' }[pol.encryption] || pol.encryption || '-';
    const reg = { closed: '관리자가 만든 계정만', approval: '가입 신청 후 관리자 승인', open: '누구나 가입' }[pol.registration] || '-';
    body = [
      h('div', { class: 'hero' }, h('h1', {}, p.tenancy?.name || p.server?.name || '릴레이 서버'), h('div', { class: 'muted' }, p.url)),
      h('div', { class: 'card' },
        h('div', { class: 'kv' },
          h('span', {}, '조직'), h('span', {}, p.tenancy ? `${p.tenancy.name || p.tenancy.id} (${p.tenancy.id})` : '-'),
          h('span', {}, '서버'), h('span', {}, p.server?.name || '-'),
          h('span', {}, '버전'), h('span', {}, p.server?.version || '-'),
          h('span', {}, '연결'), h('span', {}, `${p.ws ? 'WebSocket' : 'TCP'}${p.tls ? ' + TLS' : ''}`),
          h('span', {}, '로그인'), h('span', {}, pol.auth === 'password' ? '사용자 이름과 비밀번호' : '기기 등록만'),
          h('span', {}, '가입'), h('span', {}, reg),
          h('span', {}, '암호화'), h('span', {}, enc),
          p.fingerprint && h('span', {}, '인증서 지문'), p.fingerprint && h('span', { class: 'mono sel' }, fp(p.fingerprint))),
        !p.tenancy && h('div', { class: 'warn' }, 'Tenancy를 지원하지 않는 이전 버전 서버입니다. Tenancy ID 없이 연결합니다.'),
        trust),
      h('div', { class: 'btns' },
        h('button', { class: 'btn', onclick: () => { st.step = 1; st.err = ''; drawSetup(); } }, '뒤로'),
        h('button', { class: 'btn primary', onclick: () => { st.step = 3; st.err = ''; drawSetup(); } }, '이 서버 사용')),
    ];
  } else {
    body = drawSetupAccount(st, errEl);
  }
  put($app, h('div', { class: 'scroll' }, h('div', { class: 'pad', style: 'padding-top:calc(var(--top) + 24px)' }, steps, body)));
}

function drawSetupAccount(st, errEl) {
  const p = st.probe;
  const pol = p.policy || {};
  const password = pol.auth === 'password';
  const canRegister = password && pol.registration && pol.registration !== 'closed';
  const f = st.form || (st.form = { user: '', pass: '', pass2: '', name: S.adding ? S.settings.name || '' : '' });
  const field = (label, key, attrs = {}) => h('label', { class: 'f' }, label,
    h('input', { value: f[key], autocapitalize: 'off', autocorrect: 'off', spellcheck: false, ...attrs, oninput: (e) => { f[key] = e.target.value; } }));
  const go = async () => {
    const mode = st.mode;
    if (password && (!f.user.trim() || !f.pass)) { st.err = '사용자 이름과 비밀번호를 입력하세요'; return drawSetup(); }
    if (mode === 'register' && f.pass !== f.pass2) { st.err = '비밀번호가 서로 다릅니다'; return drawSetup(); }
    const name = (f.name.trim() || f.user.trim());
    if (!name) { st.err = '이름을 입력하세요'; return drawSetup(); }
    st.busy = true; st.err = ''; drawSetup();
    try {
      await invoke('join', { url: p.url, tls: p.tls, fingerprint: p.fingerprint || '', tenancy: st.tenancy, mode, user: f.user, pass: f.pass, name });
      f.pass = f.pass2 = '';
      if (S.adding) toast(`${p.tenancy?.name || p.server?.name || '릴레이 서버'}에 연결했습니다`);
      S.adding = false;
      S.setup = { step: 1, addr: '', tenancy: '', probe: null, mode: 'login', err: '', busy: false };
      await boot();
      return;
    } catch (e) { st.err = err(e); }
    st.busy = false;
    drawSetup();
  };
  const tabs = canRegister && h('div', { class: 'seg' },
    h('button', { class: st.mode === 'login' ? 'on' : '', onclick: () => { st.mode = 'login'; st.err = ''; drawSetup(); } }, '로그인'),
    h('button', { class: st.mode === 'register' ? 'on' : '', onclick: () => { st.mode = 'register'; st.err = ''; drawSetup(); } }, pol.registration === 'open' ? '가입' : '가입 신청'));
  const action = { login: '로그인', register: pol.registration === 'open' ? '가입하고 시작' : '가입 신청', device: '이 기기 등록' }[st.mode];
  return [
    h('div', { class: 'hero' }, h('h1', {}, p.tenancy?.name || p.server?.name || '릴레이 서버'), h('div', { class: 'muted' }, password ? '계정으로 로그인하면 이 휴대폰이 계정의 기기로 등록됩니다. 비밀번호는 저장하지 않습니다.' : '이 휴대폰을 서버에 기기로 등록합니다.')),
    tabs,
    h('div', { class: 'card' },
      password && field('사용자 이름', 'user', { enterkeyhint: 'next', autocomplete: 'username' }),
      password && field('비밀번호', 'pass', { type: 'password', autocomplete: st.mode === 'register' ? 'new-password' : 'current-password' }),
      st.mode === 'register' && field('비밀번호 확인', 'pass2', { type: 'password', autocomplete: 'new-password' }),
      field('표시 이름', 'name', { placeholder: password ? '비워 두면 사용자 이름' : '다른 사람에게 보이는 이름', autocapitalize: 'words', autocorrect: 'on' }),
      st.mode === 'register' && pol.registration === 'approval' && h('div', { class: 'muted' }, '관리자가 승인하면 앱을 쓸 수 있습니다.'),
      errEl,
      h('button', { class: 'btn primary', disabled: st.busy, onclick: go }, st.busy ? '연결하는 중…' : action)),
    h('button', { class: 'btn', onclick: () => { st.step = 2; st.err = ''; drawSetup(); } }, '뒤로'),
  ];
}

// ---------------------------------------------------------------- blocked (pending approval, rejected)

/** Every connection waits for an admin or was refused. */
function drawBlocked() {
  const r = S.relay;
  const pending = r.state === 'pending';
  const remove = async (c) => {
    if (!await ask(`'${c.label}' 연결을 지울까요?`, '지우기', true)) return;
    await invoke('unlink', { conn: c.id });
    await boot();
  };
  const add = () => { S.adding = true; route(); };
  put($app, h('div', { class: 'scroll' }, h('div', { class: 'pad', style: 'padding-top:calc(var(--top) + 48px)' },
    h('div', { class: 'hero' },
      h('div', { style: 'font-size:48px' }, pending ? '⏳' : '⚠️'),
      h('h1', {}, pending ? '관리자 승인을 기다리는 중' : '릴레이 서버에 로그온할 수 없습니다')),
    conns().map((c) => h('div', { class: 'card' },
      h('b', {}, c.label), h('div', { class: 'muted' }, c.server_name && c.server_name !== c.label ? `${c.server_name} · ${c.url}` : c.url),
      h('div', {}, c.message || (c.state === 'pending' ? '승인되면 자동으로 시작합니다.' : '')),
      h('div', { class: 'muted' }, c.state === 'pending' ? '승인되면 앱을 쓸 수 있습니다. 앱을 열어 두면 자동으로 다시 확인합니다.' : '관리자에게 계정과 기기 상태를 확인하세요.'),
      h('div', { class: 'btns' }, h('button', { class: 'btn small danger', onclick: () => remove(c) }, '연결 지우기')))),
    h('button', { class: 'btn primary', onclick: () => { invoke('reconnect', {}); toast('다시 확인하는 중…'); } }, pending ? '지금 다시 확인' : '다시 시도'),
    h('button', { class: 'btn', onclick: add }, '다른 서버 연결'))));
}

// ---------------------------------------------------------------- main (tabs)

const TABS = [
  { id: 'inbox', label: '받은 쪽지', icon: 'inbox' },
  { id: 'sent', label: '보낸 쪽지', icon: 'send' },
  { id: 'people', label: '연락처', icon: 'users' },
  { id: 'settings', label: '설정', icon: 'gear' },
];

/** One line for the connection that most needs attention. */
function connBanner() {
  const name = (c) => (multi() ? `${c.label}: ` : '');
  const bad = conns().find((c) => c.state === 'error');
  if (bad) {
    return h('div', { class: 'banner err' }, h('span', {}, `${name(bad)}연결 안 됨${bad.message ? ': ' + bad.message : ''}`),
      h('button', { onclick: () => invoke('reconnect', { conn: bad.id }) }, '다시 시도'));
  }
  const waiting = conns().find((c) => c.state === 'connecting' || c.state === 'paused' || c.state === 'off');
  if (waiting) return h('div', { class: 'banner' }, h('span', {}, `${name(waiting)}릴레이 서버에 연결하는 중…`));
  const refused = conns().find((c) => c.state === 'pending' || c.state === 'fatal');
  if (refused) return h('div', { class: 'banner err' }, h('span', {}, `${name(refused)}${refused.message || CONN[refused.state]}`));
  const note = conns().find((c) => c.state === 'online' && c.message);
  return note ? h('div', { class: 'banner' }, h('span', {}, `${name(note)}${note.message}`)) : null;
}

function drawMain() {
  const prev = $app.querySelector('.scroll');
  const keep = prev && prev.dataset.tab === S.tab ? prev.scrollTop : 0;
  const active = document.activeElement;
  const searching = active && active.id === 'search';
  const caret = searching ? active.selectionStart : 0;

  const unread = S.inbox.filter((m) => !m.read).length;
  const compose = iconBtn('pen', '쪽지 쓰기', () => openCompose());
  let title = TABS.find((t) => t.id === S.tab).label;
  let actions = [compose];
  let content;
  if (S.tab === 'inbox') {
    actions = [unread ? iconBtn('check', '모두 읽음', () => invoke('mark_all_read')) : null, compose];
    content = inboxList();
  } else if (S.tab === 'sent') {
    content = sentList();
  } else if (S.tab === 'people') {
    actions = [iconBtn('refresh', '새로 고침', () => { invoke('refresh'); toast('새로 고치는 중…'); }), compose];
    content = peopleList();
  } else {
    actions = [];
    content = settingsView();
  }
  const sub = multi() ? `릴레이 ${conns().length}개` : conns()[0]?.label || '';
  const scroll = h('div', { class: 'scroll', 'data-tab': S.tab }, content);
  put($app,
    h('div', { class: 'bar' }, h('h1', {}, title, sub && S.tab === 'inbox' && h('span', { class: 'sub' }, sub)), actions),
    connBanner(),
    S.tab === 'people' && searchBox(),
    scroll,
    h('nav', { class: 'tabs' }, TABS.map((t) => h('button', {
      class: S.tab === t.id ? 'on' : '',
      onclick: () => { if (S.tab !== t.id) { S.tab = t.id; drawMain(); } },
    }, h('span', { class: 'g' }, ic(t.icon, 22)), t.label, t.id === 'inbox' && unread ? h('span', { class: 'badge' }, unread > 99 ? '99+' : unread) : null))),
  );
  scroll.scrollTop = keep;
  if (searching) {
    const s = document.getElementById('search');
    s.focus();
    s.setSelectionRange(caret, caret);
  }
}

function searchBox() {
  return h('div', { class: 'search' },
    h('input', { id: 'search', type: 'search', placeholder: '이름, 부서, @사용자 이름 또는 주소', value: S.search, autocapitalize: 'off', autocorrect: 'off', spellcheck: false,
      oninput: (e) => { S.search = e.target.value; drawMain(); } }));
}

function inboxList() {
  if (!S.inbox.length) return h('div', { class: 'empty' }, h('div', { class: 'g' }, ic('inbox', 44)), '받은 쪽지가 없습니다',
    h('div', { class: 'muted' }, '앱이 꺼져 있는 동안 온 쪽지는 릴레이 서버가 보관했다가, 앱을 열면 받아 옵니다.'));
  return h('div', { class: 'list' }, S.inbox.map((m) => {
    const c = contactOf(m.conn, m.from);
    const preview = m.text || m.note || (m.files.length ? m.files.map((f) => f.name).join(', ') : '');
    const addr = strangerAddr(m);
    return h('div', { class: `row${m.read ? '' : ' unread'}`, onclick: () => openMessage(m.id) },
      h('span', { class: `unread-dot${m.read ? ' no' : ''}` }),
      avatar(m.from_name, c?.state),
      h('div', { class: 'main' },
        h('div', { class: 't1' }, h('b', {}, m.from_name || '알 수 없음'), addr && h('span', { class: 'handle' }, addr),
          m.group && h('span', { class: 'tag' }, m.group.name || `${m.group.members.length}명`),
          h('span', { class: 'when' }, time(m.ts))),
        h('div', { class: 't2' }, m.files.length ? h('span', { class: 'clip' }, '📎 ') : null, preview)));
  }));
}

function viaTag(r) {
  if (!r.ok) return h('span', { class: 'tag fail' }, '실패');
  return h('span', { class: `tag ${r.via}` }, r.via === 'stored' ? '보관' : '전달');
}

function sentList() {
  if (!S.sent.length) return h('div', { class: 'empty' }, h('div', { class: 'g' }, ic('send', 44)), '보낸 쪽지가 없습니다');
  return h('div', { class: 'list' }, S.sent.map((s) => {
    const names = s.to.map((r) => r.name).join(', ');
    return h('div', { class: 'row', onclick: () => openSent(s.id) },
      avatar(s.to[0]?.name),
      h('div', { class: 'main' },
        h('div', { class: 't1' }, h('b', {}, names), h('span', { class: 'when' }, time(s.ts))),
        h('div', { class: 't2' }, s.files.length ? h('span', { class: 'clip' }, '📎 ') : null, s.text || s.files.map((f) => f.name).join(', '))),
      s.to.length === 1 ? viaTag(s.to[0]) : h('span', { class: 'tag' }, `${s.to.filter((r) => r.ok).length}/${s.to.length}`));
  }));
}

function matches(c, q) {
  if (!q) return true;
  q = q.toLowerCase().replace(/^@/, '');
  return (c.name || '').toLowerCase().includes(q) || (c.username || '').toLowerCase().includes(q)
    || c.devices.some((d) => (d.name || '').toLowerCase().includes(q));
}

const deptMatches = (d, q) => !q || (d.name || '').toLowerCase().includes(q.toLowerCase()) || d.id.includes(q.toLowerCase());

/** Looks up an exact username on the servers (needed on "lookup" servers). */
async function lookupUser(q, after) {
  try {
    const c = await invoke('lookup', { query: q });
    await loadContacts();
    after?.(c);
  } catch (e) { toast(err(e)); }
}

/** Checks a typed address with its server; `after` gets a recipient. Rejections show as they come. */
async function resolveAddress(q, after) {
  toast('주소를 확인하는 중…');
  try {
    const r = await invoke('resolve', { address: q });
    after({ conn: r.conn, to: r.to, name: r.name });
  } catch (e) { toast(err(e)); }
}

/** Everyone in a department, as recipients. */
async function deptMembers(d) {
  try {
    const list = await invoke('dept_members', { conn: d.conn, id: d.id });
    await loadContacts();
    return list;
  } catch (e) {
    toast(err(e));
    return [];
  }
}

const isAddress = (q) => /^[^@\s]+.*@[^@\s]+:[^@\s]+$/.test(q);

function addressRow(q, onclick) {
  return h('div', { class: 'row', onclick },
    h('div', { class: 'avatar' }, '@'),
    h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, '주소로 보내기')),
      h('div', { class: 't2 sel' }, q)));
}

function deptRow(d, lead, onclick) {
  return h('div', { class: 'row', onclick }, lead,
    h('div', { class: 'avatar' }, '#'),
    h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, d.name || d.id), connTag(d.conn)),
      h('div', { class: 't2' }, `부서${d.members ? ` · ${d.members}명` : ''} · 모두에게 보내기`)));
}

function deviceRows(c, onPick) {
  return c.devices.map((d) => h('div', { class: 'row sub', onclick: () => onPick(`${c.account}/${d.id}`, d.name) },
    dot(d.state),
    h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, d.name || '기기'), d.host && h('span', { class: 'tag' }, d.host)),
      h('div', { class: 't2' }, STATE[d.state] + (d.key_changed ? ' · ⚠ 보안 키 바뀜' : ''))),
    h('span', { class: 'chev' }, ic('pen', 18))));
}

const expanded = new Set();
function peopleList() {
  const q = S.search.trim();
  const list = S.contacts.filter((c) => matches(c, q));
  const out = [];
  const exact = q && S.contacts.some((c) => c.username && c.username.toLowerCase() === q.toLowerCase().replace(/^@/, ''));
  if (q && isAddress(q)) {
    out.push(h('div', { class: 'list' }, addressRow(q, () => resolveAddress(q, (t) => openCompose({ to: [t] })))));
  } else if (q && !exact) {
    out.push(h('div', { class: 'list' }, h('div', { class: 'row', onclick: () => lookupUser(q, (c) => openCompose({ to: [{ conn: c.conn, to: c.account, name: c.name }] })) },
      h('div', { class: 'avatar' }, '@'),
      h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, `'${q.replace(/^@/, '')}' 사용자 찾기`)),
        h('div', { class: 't2' }, '사용자 이름 또는 계정 ID가 정확히 같아야 합니다. 다른 조직은 주소(사용자@Tenancy:서버)로 찾습니다')))));
  }
  const depts = q ? S.departments.filter((d) => deptMatches(d, q)) : [];
  if (depts.length) {
    out.push(h('div', { class: 'list' }, depts.map((d) => deptRow(d, null, async () => {
      const to = await deptMembers(d);
      if (to.length) openCompose({ to });
    }))));
  }
  if (!list.length) {
    out.push(h('div', { class: 'empty' }, h('div', { class: 'g' }, ic('users', 44)),
      q ? '일치하는 사람이 없습니다' : '연락처가 없습니다',
      !q && conns().some((c) => c.policy?.contacts === 'lookup') && h('div', { class: 'muted' }, '이 서버는 주고받은 적이 있거나 찾은 사람만 보여 줍니다. 위에서 @사용자 이름으로 찾으세요.')));
    return out;
  }
  out.push(h('div', { class: 'list' }, list.flatMap((c) => {
    const many = c.devices.length > 1 || c.me;
    const key = `${c.conn}|${c.account}`;
    const open = expanded.has(key) || c.me;
    const row = h('div', { class: 'row', onclick: () => c.me ? null : openCompose({ to: [{ conn: c.conn, to: c.account, name: c.name }] }) },
      avatar(c.name, c.state),
      h('div', { class: 'main' },
        h('div', { class: 't1' }, h('b', {}, c.name || c.username || '이름 없음'), handle(c.username), connTag(c.conn)),
        h('div', { class: 't2' }, c.me ? '기기를 골라 보낼 수 있습니다' : `${STATE[c.state]}${c.devices.length > 1 ? ` · 기기 ${c.devices.length}대` : ''}`)),
      many && !c.me && h('button', { class: 'icon', 'aria-label': '기기 보기', onclick: (e) => {
        e.stopPropagation();
        expanded.has(key) ? expanded.delete(key) : expanded.add(key);
        drawMain();
      } }, ic(open ? 'down' : 'right', 20)));
    return [row, open ? deviceRows(c, (to, name) => openCompose({ to: [{ conn: c.conn, to, name }] })) : []];
  })));
  return out;
}

// ---------------------------------------------------------------- reading

function fileCard(f) {
  const act = (action) => async () => {
    try {
      const r = await invoke('file_action', { path: f.path, action });
      if (action === 'save' && r) toast(`저장했습니다: ${r}`);
    } catch (e) {
      toast(err(e));
      // Android 9 and older cannot save to Downloads without a permission: share instead.
      if (action === 'save') invoke('file_action', { path: f.path, action: 'share' }).catch(() => {});
    }
  };
  return h('div', { class: 'file' },
    h('span', { class: 'clip' }, ic('clip', 18)),
    h('div', { class: 'main' }, h('div', { class: 'name' }, f.name), h('div', { class: 'muted' }, size(f.size))),
    f.path ? h('div', { class: 'acts' },
      h('button', { onclick: act('open') }, '열기'),
      h('button', { onclick: act('share') }, '공유'),
      h('button', { onclick: act('save') }, '저장')) : null);
}

function groupInfo(g) {
  if (!g) return null;
  return h('details', { class: 'group' },
    h('summary', {}, `${g.name ? g.name + ' · ' : ''}함께 받은 사람 ${g.members.length}명`),
    h('div', { class: 'sel' }, g.members.map((m) => m.name).join(', ')));
}

async function replyTo(id, all) {
  try {
    const to = await invoke('reply_targets', { id, all });
    if (!to.length) return toast('답장할 사람을 찾을 수 없습니다');
    openCompose({ to });
  } catch (e) { toast(err(e)); }
}

function openMessage(id) {
  const m = S.inbox.find((x) => x.id === id);
  if (!m) return;
  if (!m.read) invoke('mark_read', { id });
  const c = contactOf(m.conn, m.from);
  const addr = strangerAddr(m);
  const del = async () => {
    if (!await ask(m.files.length ? '쪽지와 받은 첨부파일을 지울까요?' : '쪽지를 지울까요?', '지우기', true)) return;
    await invoke('delete_message', { kind: 'inbox', id });
    popLayer();
  };
  const el = sheet(m.from_name || '쪽지', { right: iconBtn('trash', '지우기', del) });
  el.append(
    h('div', { class: 'scroll' }, h('div', { class: 'pad' },
      h('div', { class: 'msg-head' }, avatar(m.from_name, c?.state),
        h('div', { class: 'main' }, h('div', {}, h('b', {}, m.from_name || '알 수 없음'), ' ', handle(c?.username), connTag(m.conn)),
          addr && h('div', { class: 'muted sel' }, addr),
          h('div', { class: 'muted' }, fullTime(m.ts)))),
      groupInfo(m.group),
      m.note && h('div', { class: 'note' }, m.note),
      m.text && h('div', { class: 'body sel' }, m.text),
      m.files.length > 0 && h('div', { class: 'files' }, m.files.map(fileCard)))),
    m.from && h('div', { class: 'actions' },
      h('button', { class: 'btn primary', onclick: () => replyTo(id, false) }, '답장'),
      m.group && m.group.members.length > 1 && h('button', { class: 'btn', onclick: () => replyTo(id, true) }, '전체 답장')));
  pushLayer(el);
}

function openSent(id) {
  const s = S.sent.find((x) => x.id === id);
  if (!s) return;
  const del = async () => {
    if (!await ask('보낸 쪽지 기록을 지울까요?', '지우기', true)) return;
    await invoke('delete_message', { kind: 'sent', id });
    popLayer();
  };
  const el = sheet('보낸 쪽지', { right: iconBtn('trash', '지우기', del) });
  el.append(
    h('div', { class: 'scroll' }, h('div', { class: 'pad' },
      h('div', { class: 'muted' }, fullTime(s.ts)),
      h('div', { class: 'card' }, h('div', { class: 'results' }, s.to.map((r) => h('div', {},
        viaTag(r), h('b', {}, r.name), connTag(r.conn),
        h('span', { class: 'muted' }, r.ok ? (r.via === 'stored' ? '앱이 꺼져 있어 서버에 보관, 접속하면 전달' : '전달됨') + (r.error ? ` · ${r.error}` : '') : r.error))))),
      s.text && h('div', { class: 'body sel' }, s.text),
      s.files.length > 0 && h('div', { class: 'files' }, s.files.map(fileCard)))),
    h('div', { class: 'actions' },
      h('button', { class: 'btn', onclick: () => openCompose({ to: s.to.map((r) => ({ conn: r.conn, to: r.to, name: r.name })), text: s.text }) }, '다시 쓰기')));
  pushLayer(el);
}

// ---------------------------------------------------------------- incoming popup

function showPopup(m) {
  if (layers.some((L) => L.popup)) {
    S.popups.push(m);
    layers.find((L) => L.popup).refresh();
    return;
  }
  S.popups = [m];
  let L;
  const card = h('div', { class: 'popup' });
  const draw = () => {
    const cur = S.popups[0];
    const c = contactOf(cur.conn, cur.from);
    const addr = strangerAddr(cur);
    const next = async (fn) => {
      S.popups.shift();
      if (S.popups.length) return draw();
      await popLayer();
      fn?.();
    };
    put(card,
      h('div', { class: 'head' }, h('div', { class: 'msg-head' }, avatar(cur.from_name, c?.state),
        h('div', { class: 'main' }, h('b', {}, cur.from_name || '알 수 없음'), ' ', handle(c?.username),
          addr && h('div', { class: 'muted' }, addr),
          h('div', { class: 'muted' }, fullTime(cur.ts), S.popups.length > 1 ? h('span', { class: 'more' }, ` · 쪽지 ${S.popups.length - 1}개 더`) : null)))),
      h('div', { class: 'content' },
        groupInfo(cur.group),
        cur.note && h('div', { class: 'note' }, cur.note),
        cur.text && h('div', { class: 'body sel' }, cur.text),
        cur.files.length > 0 && h('div', { class: 'muted' }, `📎 첨부파일 ${cur.files.length}개`)),
      h('div', { class: 'btns' },
        h('button', { class: 'btn', onclick: () => { invoke('mark_read', { id: cur.id }); next(); } }, '닫기'),
        cur.files.length > 0 && h('button', { class: 'btn', onclick: () => next(() => openMessage(cur.id)) }, '열기'),
        cur.from && h('button', { class: 'btn primary', onclick: () => { invoke('mark_read', { id: cur.id }); next(() => replyTo(cur.id, false)); } }, '답장')));
  };
  draw();
  const el = h('div', { class: 'layer modal' }, card);
  L = pushLayer(el, { popup: true, refresh: draw, onClose: () => { S.popups = []; } });
}

// ---------------------------------------------------------------- writing

/** Attachment limits for recipients: the tightest of the connections they go through. */
function fileLimits(to) {
  const ids = [...new Set(to.map((t) => connId(t.conn)))];
  const list = (ids.length ? ids.map(connOf) : conns().slice(0, 1)).filter(Boolean);
  if (!list.length) return { maxFiles: 0, maxSize: 0 };
  return {
    maxFiles: Math.min(...list.map((c) => c.policy?.relay_max_files ?? 0)),
    maxSize: Math.min(...list.map((c) => c.policy?.relay_max_file_size ?? 0)),
  };
}

/** Copies a picked file into the app (the webview only has it as a Blob). */
function blobB64(blob) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).slice(String(r.result).indexOf(',') + 1));
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}

async function stageFile(file, entry, redraw) {
  const CH = 1024 * 1024;
  entry.token = await invoke('stage_open', { name: file.name });
  for (let off = 0; off < file.size; off += CH) {
    if (entry.removed) return;
    await invoke('stage_write', { token: entry.token, data: await blobB64(file.slice(off, off + CH)) });
    entry.done = Math.min(off + CH, file.size);
    redraw();
  }
  entry.ready = true;
  redraw();
}

function openCompose(init = {}) {
  const sig = S.settings.signature ? `\n\n${S.settings.signature}` : '';
  const c = {
    to: (init.to || []).slice(),
    text: init.text != null ? init.text : sig,
    files: [],
    reveal: false,
    sending: false,
    progress: null,
    results: null,
    job: uid(),
  };
  let L;
  const text = h('textarea', { class: 'compose-text', placeholder: '내용', value: c.text, oninput: (e) => { c.text = e.target.value; drawBar(); } });
  const picker = h('input', { type: 'file', multiple: true, style: 'display:none', onchange: (e) => { addFiles([...e.target.files]); e.target.value = ''; } });
  const sendBtn = h('button', { class: 'icon text', onclick: () => send() }, '보내기');
  const toRow = h('div', { class: 'to' });
  const filesEl = h('div', { class: 'files', style: 'padding:0 12px' });
  const extra = h('div', {});

  const drawBar = () => {
    const staging = c.files.some((f) => !f.ready && !f.error);
    sendBtn.disabled = c.sending || !c.to.length || staging || (!c.text.trim() && !c.files.length) || c.files.some((f) => f.error);
  };
  const drawTo = () => {
    put(toRow, h('span', { class: 'lbl' }, '받는 사람'),
      c.to.map((t, i) => h('span', { class: 'chip' }, h('span', {}, multi() && connOf(t.conn) ? `${t.name} · ${connOf(t.conn).label}` : t.name),
        h('button', { 'aria-label': '빼기', onclick: () => { c.to.splice(i, 1); drawTo(); drawExtra(); drawBar(); } }, '✕'))),
      h('button', { class: 'chip add', onclick: () => openPicker(c.to, (sel) => { c.to = sel; drawTo(); drawExtra(); drawBar(); }) }, '+ 추가'));
  };
  const drawFiles = () => {
    put(filesEl, ...c.files.map((f, i) => h('div', { class: 'file' },
      h('span', { class: 'clip' }, ic('clip', 18)),
      h('div', { class: 'main' }, h('div', { class: 'name' }, f.name),
        f.error ? h('div', { class: 'err' }, f.error)
          : f.ready ? h('div', { class: 'muted' }, size(f.size))
            : h('progress', { max: f.size || 1, value: f.done || 0 })),
      h('button', { class: 'icon', 'aria-label': '빼기', onclick: () => {
        f.removed = true;
        if (f.token) invoke('stage_drop', { token: f.token });
        c.files.splice(i, 1);
        drawFiles(); drawBar();
      } }, ic('x', 18)))));
  };
  const drawExtra = () => {
    const { maxFiles, maxSize } = fileLimits(c.to);
    put(extra,
      c.to.length > 1 && h('label', { class: 'switch', style: 'padding:10px 16px' }, h('span', {}, '받는 사람끼리 서로 보이기 (전체 답장 가능)'),
        h('input', { type: 'checkbox', checked: c.reveal, onchange: (e) => { c.reveal = e.target.checked; } })),
      h('div', { class: 'limits' }, maxFiles ? `첨부파일 최대 ${maxFiles}개 · 파일당 ${size(maxSize)}` : '이 서버는 첨부파일을 중계하지 않습니다'),
      c.progress && h('div', { class: 'progress' }, c.progress.label, c.progress.total ? h('progress', { max: c.progress.total, value: c.progress.done }) : h('progress', {})),
      c.results && h('div', { class: 'pad', style: 'padding-top:0;padding-bottom:0' }, h('div', { class: 'card' },
        h('b', {}, c.results.ok ? '일부 받는 사람에게 보내지 못했습니다' : '보내지 못했습니다'),
        h('div', { class: 'results' }, c.results.to.map((r) => h('div', {}, viaTag(r), h('b', {}, r.name), h('span', { class: 'muted' }, r.error)))))));
  };
  const addFiles = (list) => {
    const { maxFiles, maxSize } = fileLimits(c.to);
    if (!maxFiles) return toast('이 서버는 첨부파일을 중계하지 않습니다');
    if (c.files.length + list.length > maxFiles) return toast(`첨부파일은 ${maxFiles}개까지 보낼 수 있습니다`);
    for (const file of list) {
      const entry = { name: file.name, size: file.size, done: 0 };
      if (maxSize && file.size > maxSize) entry.error = `파일당 ${size(maxSize)}까지 보낼 수 있습니다`;
      c.files.push(entry);
      if (!entry.error) {
        stageFile(file, entry, () => { drawFiles(); drawBar(); }).catch((e) => { entry.error = err(e); drawFiles(); drawBar(); });
      }
    }
    drawFiles();
    drawBar();
  };
  const send = async () => {
    if (!c.to.some((t) => connOf(t.conn)?.state === 'online')) return toast('릴레이 서버에 연결되어 있지 않습니다');
    c.sending = true;
    c.results = null;
    c.progress = { label: '보내는 중…', done: 0, total: 0 };
    drawBar(); drawExtra();
    try {
      const to = c.to.map((t) => ({ conn: t.conn || '', to: t.to, name: t.name || '' }));
      const r = await invoke('send', { msg: { job: c.job, to, text: c.text, files: c.files.map((f) => f.token), reveal: c.reveal } });
      c.progress = null;
      if (r.to.every((x) => x.ok && !x.error)) {
        c.files = []; // sent and cleaned up: nothing to drop
        await popLayer();
        const stored = r.to.filter((x) => x.via === 'stored').length;
        toast(stored === r.to.length ? '보냈습니다 · 상대 앱이 열리면 전달됩니다' : '보냈습니다');
        return;
      }
      c.results = r;
      // Successful recipients are done; the failed ones stay for another try.
      c.to = c.to.filter((t) => !r.to.find((x) => rkey(x) === rkey(t) && x.ok));
    } catch (e) {
      c.progress = null;
      toast(err(e));
    }
    c.sending = false;
    c.job = uid();
    drawTo(); drawExtra(); drawBar();
  };

  const el = sheet('쪽지 쓰기', { right: sendBtn });
  el.append(toRow,
    h('div', { class: 'scroll', style: 'display:flex;flex-direction:column;padding-bottom:0' }, text, filesEl, extra),
    h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => picker.click() }, ic('clip', 18), ' 첨부'), picker));
  L = pushLayer(el, {
    progress: (p) => {
      if (p.job !== c.job || !c.progress) return;
      c.progress = { label: `올리는 중 ${size(p.done)} / ${size(p.total)}`, done: p.done, total: p.total };
      drawExtra();
    },
    onClose: () => { for (const f of c.files) { f.removed = true; if (f.token) invoke('stage_drop', { token: f.token }); } },
  });
  drawTo(); drawFiles(); drawExtra(); drawBar();
  if (c.to.length) {
    requestAnimationFrame(() => { text.focus(); text.setSelectionRange(0, 0); });
  } else {
    openPicker(c.to, (sel) => { c.to = sel; drawTo(); drawExtra(); drawBar(); requestAnimationFrame(() => text.focus()); });
  }
}

/**
 * Multi-select of people (whole accounts), single devices, departments (all of
 * their members) and typed addresses. The mobile app has no groups of its own.
 */
function openPicker(current, done) {
  const sel = current.slice();
  let q = '';
  const listEl = h('div', { class: 'scroll' });
  const has = (conn, to) => sel.some((t) => rkey(t) === rkey({ conn, to }));
  const add = (t) => {
    // A whole account and one of its devices would be the same person twice.
    const acc = t.to.includes('@') ? null : t.to.split('/')[0];
    for (let j = sel.length - 1; j >= 0 && acc; j--) {
      const s = sel[j];
      if (connId(s.conn) !== connId(t.conn) || s.to.includes('@')) continue;
      const a = s.to.split('/')[0];
      if (a === acc && (t.to === acc || s.to === acc)) sel.splice(j, 1);
    }
    if (!sel.some((s) => rkey(s) === rkey(t))) sel.push(t);
  };
  const toggle = (conn, to, name) => {
    const i = sel.findIndex((t) => rkey(t) === rkey({ conn, to }));
    if (i >= 0) sel.splice(i, 1);
    else add({ conn, to, name });
    draw();
  };
  /** A department adds everyone in it; tapped again with all of them in, it takes them out. */
  const toggleDept = async (d) => {
    const list = await deptMembers(d);
    if (!list.length) return;
    const all = list.every((t) => has(t.conn, t.to));
    if (all) {
      for (const t of list) sel.splice(sel.findIndex((s) => rkey(s) === rkey(t)), 1);
      toast(`${d.name || d.id}: ${list.length}명을 뺐습니다`);
    } else {
      list.forEach(add);
      toast(`${d.name || d.id}: ${list.length}명을 넣었습니다`);
    }
    draw();
  };
  const check = (on) => h('span', { class: `check${on ? ' on' : ''}` }, on ? '✓' : '');
  const draw = () => {
    doneBtn.textContent = sel.length ? `완료 (${sel.length})` : '완료';
    const list = S.contacts.filter((c) => matches(c, q));
    const rows = [];
    // Addresses picked earlier are not in the contacts; keep them in view.
    for (const t of sel.filter((t) => t.to.includes('@'))) {
      rows.push(h('div', { class: 'row', onclick: () => toggle(t.conn, t.to, t.name) },
        check(true), h('div', { class: 'avatar' }, '@'),
        h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, t.name), connTag(t.conn)), h('div', { class: 't2' }, t.to))));
    }
    const exact = q && S.contacts.some((c) => c.username && c.username.toLowerCase() === q.toLowerCase().replace(/^@/, ''));
    if (q && isAddress(q)) {
      if (!sel.some((t) => t.to === q)) rows.push(addressRow(q, () => resolveAddress(q, (t) => { add(t); draw(); })));
    } else if (q && !exact) {
      rows.push(h('div', { class: 'row', onclick: () => lookupUser(q, (c) => { toggle(c.conn, c.account, c.name); }) },
        h('div', { class: 'avatar' }, '@'), h('div', { class: 'main' }, h('b', {}, `'${q.replace(/^@/, '')}' 사용자 찾기`))));
    }
    const depts = S.departments.filter((d) => deptMatches(d, q));
    if (depts.length) {
      rows.push(h('div', { class: 'section' }, '부서'));
      for (const d of depts) {
        const members = S.contacts.filter((c) => !c.me && c.conn === d.conn && (c.departments || []).includes(d.id));
        rows.push(deptRow(d, check(members.length > 0 && members.every((c) => has(c.conn, c.account))), () => toggleDept(d)));
      }
      if (list.length) rows.push(h('div', { class: 'section' }, '사람'));
    }
    for (const c of list) {
      const key = `${c.conn}|${c.account}`;
      if (!c.me) {
        const open = expanded.has(key);
        rows.push(h('div', { class: 'row', onclick: () => toggle(c.conn, c.account, c.name) },
          check(has(c.conn, c.account)), avatar(c.name, c.state),
          h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, c.name), handle(c.username), connTag(c.conn)),
            h('div', { class: 't2' }, STATE[c.state] + (c.devices.length > 1 ? ` · 모든 기기 ${c.devices.length}대` : ''))),
          c.devices.length > 1 && h('button', { class: 'icon', 'aria-label': '기기 고르기', onclick: (e) => {
            e.stopPropagation();
            open ? expanded.delete(key) : expanded.add(key);
            draw();
          } }, ic(open ? 'down' : 'right', 20))));
        if (!open) continue;
      } else {
        rows.push(h('div', { class: 'section' }, multi() ? `내 다른 기기 · ${connOf(c.conn)?.label || ''}` : '내 다른 기기'));
      }
      for (const d of c.devices) {
        const to = `${c.account}/${d.id}`;
        rows.push(h('div', { class: 'row sub', onclick: () => toggle(c.conn, to, c.me ? d.name || '내 기기' : `${c.name} (${d.name || d.host || '기기'})`) },
          check(has(c.conn, to)), dot(d.state),
          h('div', { class: 'main' }, h('div', { class: 't1' }, h('b', {}, d.name || '기기'), d.host && h('span', { class: 'tag' }, d.host)),
            h('div', { class: 't2' }, STATE[d.state]))));
      }
    }
    if (!rows.length) rows.push(h('div', { class: 'empty' }, q ? '일치하는 사람이 없습니다' : '연락처가 없습니다'));
    put(listEl, h('div', { class: 'list' }, rows));
  };
  const doneBtn = h('button', { class: 'icon text', onclick: async () => { await popLayer(); done(sel); } }, '완료');
  const el = sheet('받는 사람', { right: doneBtn });
  el.append(
    h('div', { class: 'search' }, h('input', { type: 'search', placeholder: '이름, 부서, @사용자 이름 또는 주소', autocapitalize: 'off', autocorrect: 'off', spellcheck: false,
      oninput: (e) => { q = e.target.value.trim(); draw(); } })),
    listEl);
  pushLayer(el, { refresh: draw });
  draw();
}

// ---------------------------------------------------------------- settings

async function saveSettings(patch) {
  try {
    S.settings = await invoke('set_settings', { s: { ...S.settings, ...patch } });
  } catch (e) { toast(err(e)); }
  drawMain();
}

function seg(options, value, onpick) {
  return h('div', { class: 'seg' }, options.map(([v, label]) => h('button', { class: v === value ? 'on' : '', onclick: () => v !== value && onpick(v) }, label)));
}

const CONN = { online: '연결됨', connecting: '연결 중', paused: '대기', error: '연결 안 됨', off: '꺼짐', pending: '승인 대기', fatal: '로그온 실패' };

const HOLD_NOTE = {
  allow: '앱이 꺼져 있는 동안 온 쪽지는 서버가 보관했다가 앱을 열면 전달합니다.',
  device_off: '앱이 꺼져 있으면 이 휴대폰으로는 쪽지를 받을 수 없습니다 (내 다른 기기는 그대로).',
  all_off: '앱이 꺼져 있으면 쪽지를 받을 수 없습니다. 내 모든 기기에 적용됩니다.',
};

/** One relay connection: where it is, how it is doing, and its own settings. */
function connCard(r) {
  const pol = r.policy || {};
  const setHold = async (mode) => {
    try { await invoke('hold', { conn: r.id, mode }); } catch (e) { toast(err(e)); }
  };
  const remove = async () => {
    const last = conns().length === 1;
    const text = last
      ? '마지막 릴레이 연결입니다. 지우면 다시 연결하기 전까지 앱을 쓸 수 없습니다. 받은 쪽지는 이 휴대폰에 남습니다.'
      : `'${r.label}' 연결을 지울까요? 이 연결의 연락처로는 더 보낼 수 없습니다. 받은 쪽지는 이 휴대폰에 남습니다.`;
    if (!await ask(text, last ? '나가기' : '지우기', true)) return;
    await invoke('unlink', { conn: r.id });
    await boot();
  };
  return h('div', { class: 'card' },
    h('h2', {}, dot({ online: 'online', pending: 'dnd', fatal: 'off', error: 'off' }[r.state] || 'hold'), ' ', r.label),
    h('div', { class: 'kv' },
      h('span', {}, '조직'), h('span', {}, r.tenancy ? `${r.tenancy_name || r.tenancy} (${r.tenancy})` : '확인 전'),
      h('span', {}, '서버'), h('span', {}, r.server_name || '-'),
      h('span', {}, '주소'), h('span', { class: 'sel' }, r.url),
      h('span', {}, '연결'), h('span', {}, CONN[r.state] || r.state, r.message ? ` · ${r.message}` : ''),
      r.username && h('span', {}, '계정'), r.username && h('span', {}, `@${r.username}`),
      r.address && h('span', {}, '내 주소'), r.address && h('span', { class: 'mono sel' }, r.address),
      h('span', {}, '암호화'), h('span', {}, { plain: '없음', tls: 'TLS', e2ee: '종단간 암호화' }[pol.encryption] || '-'),
      r.my_key && h('span', {}, '내 키 지문'), r.my_key && h('span', { class: 'mono sel' }, fp(r.my_key)),
      r.fingerprint && h('span', {}, '서버 인증서'), r.fingerprint && h('span', { class: 'mono sel' }, fp(r.fingerprint).slice(0, 39) + '…'),
      h('span', {}, '서버 버전'), h('span', {}, r.server_version || '-')),
    h('div', { class: 'muted' }, '앱이 꺼져 있을 때 받기'),
    r.hold == null
      ? h('div', { class: 'muted' }, '이 서버는 이 설정을 지원하지 않습니다. 앱이 꺼져 있는 동안의 쪽지는 서버가 보관합니다.')
      : [seg([['allow', '서버에 보관'], ['device_off', '이 기기 끄기'], ['all_off', '모든 기기 끄기']], r.hold, setHold),
        h('div', { class: r.hold === 'allow' ? 'muted' : 'warn' }, HOLD_NOTE[r.hold] || '')],
    h('div', { class: 'muted' }, `보관 기간: 본문 ${pol.retention_hours?.text ?? '-'}시간, 첨부파일 ${pol.retention_hours?.files ?? '-'}시간.`),
    h('div', { class: 'btns' },
      h('button', { class: 'btn small', onclick: () => { invoke('reconnect', { conn: r.id }); toast('다시 연결하는 중…'); } }, '다시 연결'),
      h('button', { class: 'btn small danger', onclick: remove }, conns().length === 1 ? '서버에서 나가기' : '연결 지우기')));
}

function settingsView() {
  const s = S.settings;
  const r = S.relay;
  const changed = conns().flatMap((c) => (c.key_changed || []).map((k) => ({ ...k, conn: c.id })));
  const add = () => { S.adding = true; route(); };
  const me = conns().find((c) => c.username)?.username;
  return h('div', { class: 'pad' },
    h('div', { class: 'card', onclick: openProfile, style: 'flex-direction:row;align-items:center;gap:12px' },
      avatar(s.name, { online: 'online', dnd: 'dnd', offline: 'off' }[s.status]),
      h('div', { class: 'main', style: 'flex:1;min-width:0' }, h('b', {}, s.name), ' ', handle(me),
        h('div', { class: 'muted' }, '이름, 서명, 받기 한도')),
      h('span', { class: 'chev' }, ic('right', 20))),
    h('div', { class: 'card' },
      h('h2', {}, '상태'),
      seg([['online', '온라인'], ['dnd', '방해금지'], ['offline', '숨김']], s.status, (v) => saveSettings({ status: v })),
      h('div', { class: 'muted' }, { online: '쪽지가 오면 바로 보여 줍니다.', dnd: '보내는 사람에게 방해금지로 보이고, 쪽지는 알림 없이 쪽지함에 쌓입니다.', offline: '다른 사람 목록에 접속 중으로 보이지 않습니다. 쪽지는 받습니다.' }[s.status]),
      h('label', { class: 'switch' }, h('span', {}, '받으면 바로 화면에 띄우기'),
        h('input', { type: 'checkbox', checked: s.popup, onchange: (e) => saveSettings({ popup: e.target.checked }) }))),
    changed.length > 0 && h('div', { class: 'card' },
      h('h2', {}, '⚠ 보안 키가 바뀐 기기'),
      h('div', { class: 'muted' }, '상대가 앱을 다시 설치했거나 기기를 바꾼 경우입니다. 상대에게 키 지문을 확인한 뒤 확인을 누르세요. 그 전까지 이 기기로는 보내지 않습니다.'),
      changed.map((k) => h('div', { class: 'kv' }, h('span', {}, k.name, connTag(k.conn)), h('span', { class: 'mono sel' }, fp(k.fingerprint)))),
      h('button', { class: 'btn', onclick: () => invoke('accept_keys') }, '확인했습니다')),
    h('div', { class: 'section' }, multi() ? `릴레이 연결 ${conns().length}개` : '릴레이 서버'),
    conns().map(connCard),
    h('div', { class: 'card' },
      h('div', { class: 'muted' }, '이 앱은 배터리를 아끼려고 화면에 없을 때 연결을 끊습니다. 여러 서버나 같은 서버의 여러 조직(Tenancy)에 함께 연결할 수 있습니다.'),
      h('div', { class: 'kv' }, h('span', {}, '기기 ID'), h('span', { class: 'mono sel' }, r.device)),
      h('button', { class: 'btn', onclick: add }, '+ 릴레이 연결 추가')),
    h('div', { class: 'version' }, `PopupMessenger ${S.version} · ${S.platform}`));
}

function openProfile() {
  const s = { ...S.settings };
  const field = (label, input) => h('label', { class: 'f' }, label, input);
  const save = async () => {
    try {
      S.settings = await invoke('set_settings', { s: { ...s, max_files: Number(s.max_files) || 0, max_file_mb: Number(s.max_file_mb) || 1 } });
      await popLayer();
      drawMain();
      toast('저장했습니다');
    } catch (e) { toast(err(e)); }
  };
  const el = sheet('내 정보', { right: textBtn('저장', save) });
  el.append(h('div', { class: 'scroll' }, h('div', { class: 'pad' },
    h('div', { class: 'card' },
      field('표시 이름', h('input', { value: s.name, maxlength: 40, oninput: (e) => { s.name = e.target.value; } })),
      field('서명 (새 쪽지와 답장 끝에 붙음)', h('textarea', { value: s.signature, style: 'min-height:90px', oninput: (e) => { s.signature = e.target.value; } }))),
    h('div', { class: 'card' },
      h('h2', {}, '받기 한도'),
      h('div', { class: 'muted' }, '보내는 쪽 앱과 서버가 이 한도를 넘는 첨부파일을 거절합니다.'),
      field('쪽지 하나의 첨부파일 수', h('input', { type: 'number', inputmode: 'numeric', min: 0, max: 1000, value: s.max_files, oninput: (e) => { s.max_files = e.target.value; } })),
      field('파일 하나의 크기 (MB)', h('input', { type: 'number', inputmode: 'numeric', min: 1, value: s.max_file_mb, oninput: (e) => { s.max_file_mb = e.target.value; } }))))));
  pushLayer(el);
}

// ---------------------------------------------------------------- start

async function boot() {
  const b = await invoke('boot');
  S.settings = b.settings;
  S.relay = b.relay;
  S.version = b.version;
  S.platform = b.platform;
  await Promise.all([loadMessages(), loadContacts()]);
  route();
}

let redrawQueued = false;
function redraw() {
  if (redrawQueued) return;
  redrawQueued = true;
  requestAnimationFrame(() => {
    redrawQueued = false;
    if (S.screen === 'main') drawMain();
    refreshLayers();
  });
}

const visible = () => document.visibilityState === 'visible';
document.addEventListener('visibilitychange', () => invoke('foreground', { on: visible() }));

listen('relay', async () => {
  const was = S.relay.state;
  await loadRelay();
  if (S.screen === 'setup' && (!S.relay.linked || S.adding)) return; // do not disturb typing
  if (S.relay.state === 'online' && was !== 'online') await loadContacts();
  route();
  refreshLayers();
});
listen('contacts', async () => {
  if (!S.relay.linked) return;
  await loadContacts();
  redraw();
});
listen('messages', async () => {
  await loadMessages();
  redraw();
});
listen('message', async (e) => {
  await loadMessages();
  redraw();
  const m = S.inbox.find((x) => x.id === e.payload.message.id) || e.payload.message;
  if (e.payload.popup && visible() && S.screen === 'main') showPopup(m);
});
listen('progress', (e) => {
  for (const L of layers) L.progress?.(e.payload);
});

invoke('foreground', { on: visible() });
invoke('insets');
boot().catch((e) => { put($app, h('div', { class: 'pad' }, h('div', { class: 'err' }, err(e)))); });
