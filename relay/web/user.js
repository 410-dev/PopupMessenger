'use strict';
// PopupMessenger relay: self-service page for account holders (relay port).

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

const pad = (n) => String(n).padStart(2, '0');
function ts(t) {
  if (!t) return '—';
  const d = new Date(t);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
function ago(t) {
  if (!t) return '—';
  const s = Math.floor((Date.now() - t) / 1000);
  if (s < 60) return '방금';
  if (s < 3600) return `${Math.floor(s / 60)}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}
function bytes(n) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(1)} ${u[i]}` : `${n} B`;
}
const num = (n) => (Number(n) || 0).toLocaleString('ko-KR');

async function api(method, path, body) {
  const opt = { method, credentials: 'same-origin', headers: { 'X-Relay-User': '1' } };
  if (body !== undefined) {
    opt.headers['Content-Type'] = 'application/json';
    opt.body = JSON.stringify(body);
  }
  const r = await fetch(path, opt);
  let j = {};
  try { j = await r.json(); } catch { /* empty */ }
  if (r.status === 401 && path !== '/api/me/login') { start(); throw new Error(j.error || '로그인이 필요합니다'); }
  if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
  return j;
}

const toasts = h('div', { class: 'toasts' });
document.body.append(toasts);
function toast(msg, err) {
  const t = h('div', { class: 'toast' + (err ? ' er' : '') }, msg);
  toasts.append(t);
  setTimeout(() => t.remove(), err ? 6000 : 3000);
}
const fail = (e) => toast(String(e.message || e), true);
const field = (label, input) => h('div', { class: 'fld' }, h('label', { class: 'mu' }, label), input);
const chip = (cls, text) => h('span', { class: 'chip ' + cls }, text);

const ACTIONS = {
  register: '가입', device_add: '기기 등록', device_move: '기기를 이 계정으로 이동', login_locked: '로그인 잠김',
  user_login: '웹 로그인', user_login_failed: '웹 로그인 실패', user_password: '비밀번호 변경', user_name: '이름 변경',
  user_device_kick: '기기 연결 끊기', user_device_delete: '기기 등록 해제',
  api_key_create: 'API 키 발급', api_key_revoke: 'API 키 폐기', relay_receive: '릴레이 수신 설정',
  account_moved_in: '다른 Tenancy에서 옮겨 옴',
};
const TEN_KEY = 'relay-portal-tenancy';
const ENC = { plain: '평문', tls: '서버 통신 암호화 (TLS)', e2ee: '종단간 암호화' };

let S = null; // session info

async function start() {
  S = await fetch('/api/me/session', { credentials: 'same-origin' }).then((r) => r.json());
  document.title = `${S.server_name} · 내 계정`;
  if (S.user) return home();
  let saved = '';
  try { saved = localStorage.getItem(TEN_KEY) || ''; } catch { /* private mode */ }
  const tenancy = h('input', { type: 'text', autocomplete: 'organization', value: saved, placeholder: '예: example' });
  const user = h('input', { type: 'text', autocomplete: 'username' });
  const pass = h('input', { type: 'password', autocomplete: 'current-password' });
  const err = h('div', { class: 'err' });
  const btn = h('button', { class: 'btn pri' }, '로그인');
  async function go(e) {
    e.preventDefault();
    err.textContent = '';
    btn.disabled = true;
    const t = tenancy.value.trim().toLowerCase();
    try {
      await api('POST', '/api/me/login', { tenancy: t, username: user.value.trim(), password: pass.value });
      try { localStorage.setItem(TEN_KEY, t); } catch { /* private mode */ }
      await start();
    } catch (x) { err.textContent = String(x.message || x); btn.disabled = false; }
  }
  $app.replaceChildren(h('div', { class: 'center' }, h('form', { class: 'card', onsubmit: go },
    h('div', { class: 'mu' }, `${S.server_name} · PopupMessenger 릴레이`),
    h('h1', {}, '내 계정 관리'),
    !S.secure && h('div', { class: 'warnbox' }, '암호화되지 않은 연결입니다. 평문을 허용하지 않는 Tenancy에는 로그인할 수 없습니다. 가능하면 https:// 주소로 다시 접속하세요.'),
    field('Tenancy ID', tenancy),
    field('사용자 이름', user), field('비밀번호', pass), err, btn,
    h('div', { class: 'mu' }, 'PopupMessenger 앱에서 릴레이 서버를 연결할 때 쓴 Tenancy ID, 사용자 이름, 비밀번호로 로그인합니다. 앱에 Tenancy ID 입력란이 없었다면 비워 두세요.'))));
  (saved ? user : tenancy).focus();
}

async function home() {
  const top = h('header', { class: 'ptop' },
    h('b', {}, S.server_name), S.tenancy && h('span', {}, `${S.tenancy.name} (${S.tenancy.id})`), h('span', { class: 'mu' }, '내 계정'), h('span', { class: 'sp' }),
    h('span', {}, S.user),
    h('button', { class: 'btn sm', onclick: async () => { try { await api('POST', '/api/me/logout'); } catch { /* */ } start(); } }, '로그아웃'));
  const body = h('main', { class: 'portal' });
  $app.replaceChildren(top, body);
  await draw(body);
}

async function draw(body) {
  const d = await api('GET', '/api/me');
  const a = d.account, p = d.policy;
  const name = h('input', { type: 'text', value: a.name });
  const saveName = async () => {
    try { await api('PATCH', '/api/me', { name: name.value }); toast('이름을 바꿨습니다'); } catch (e) { fail(e); }
  };
  const online = d.devices.filter((x) => x.online).length;
  body.replaceChildren(
    h('section', { class: 'panel' }, h('div', { class: 'hd' }, '계정'), h('div', { class: 'bd' }, h('div', { class: 'kv' },
      h('div', {}, '사용자 이름'), h('div', {}, h('b', {}, a.username || '—')),
      h('div', {}, '표시 이름'), h('div', { class: 'inl' }, name, h('button', { class: 'btn sm', onclick: saveName }, '저장')),
      h('div', {}, '계정 ID'), h('div', { class: 'mono' }, a.id),
      h('div', {}, '가입'), h('div', {}, ts(a.created)),
      h('div', {}, '마지막 접속'), h('div', {}, a.last_seen ? `${ts(a.last_seen)} (${ago(a.last_seen)})` : '—'),
      h('div', {}, '기기'), h('div', {}, `${d.devices.length} / ${a.max_devices}대 등록 · ${online}대 접속 중`),
      h('div', {}, '받을 쪽지'), h('div', {}, `${num(a.pending)}건 (기기 단위)`),
      h('div', {}, '보낸 첨부 보관'), h('div', {}, `${bytes(a.stored_bytes)}${a.quota_mb ? ` / ${num(a.quota_mb)} MB` : ''}`),
      h('div', {}, '서버 정책'), h('div', {}, `${ENC[p.encryption] || p.encryption} · 첨부 ${p.relay_max_files ? `${p.relay_max_files}개, 파일당 ${bytes(p.relay_max_file_size)}` : '중계 안 함'} · 보관 ${p.retention_hours.text}시간`)))),
    devicesPanel(d.devices, () => draw(body)),
    await keysPanel(() => draw(body)),
    passwordPanel(p.min_password_len),
    activityPanel());
}

// API keys: bearer tokens for scripts, acting as this account.
async function keysPanel(redraw) {
  const r = await api('GET', '/api/me/keys');
  if (!r.allowed && !r.items.length) return null;
  const name = h('input', { type: 'text', placeholder: '예: 빌드 알림' });
  const send = h('input', { type: 'checkbox', checked: true });
  const recv = h('input', { type: 'checkbox' });
  const shown = h('div');
  async function create() {
    try {
      const k = await api('POST', '/api/me/keys', { name: name.value.trim(), send: send.checked, receive: recv.checked });
      const t = h('input', { type: 'text', readonly: true, value: k.token, class: 'mono wide', onfocus: (e) => e.target.select() });
      shown.replaceChildren(h('div', { class: 'warnbox' }, '새 API 키입니다. 지금 한 번만 보여 줍니다. 안전한 곳에 보관하세요.', t,
        h('button', { class: 'btn sm', onclick: () => navigator.clipboard.writeText(k.token).then(() => toast('복사했습니다')) }, '복사')));
      name.value = '';
      list.replaceChildren(table((await api('GET', '/api/me/keys')).items));
    } catch (e) { fail(e); }
  }
  async function revoke(k) {
    if (!confirm(`"${k.name}" 키를 폐기할까요?\n이 키를 쓰는 스크립트는 더는 동작하지 않습니다.`)) return;
    try { await api('DELETE', `/api/me/keys/${k.id}`); toast('폐기했습니다'); redraw(); } catch (e) { fail(e); }
  }
  const table = (items) => (items.length
    ? h('div', { class: 'tw' }, h('table', { class: 'g' },
      h('thead', {}, h('tr', {}, ['이름', '권한', '받을 쪽지', '마지막 사용', '발급', ''].map((t) => h('th', {}, t)))),
      h('tbody', {}, items.map((k) => h('tr', {},
        h('td', {}, k.name),
        h('td', {}, k.send && chip('ac', '보내기'), ' ', k.receive && chip('ok', '받기')),
        h('td', { class: 'num' }, k.receive ? num(k.pending) : ''),
        h('td', {}, k.last_used ? `${ts(k.last_used)} (${k.last_ip})` : '—'),
        h('td', {}, ts(k.created)),
        h('td', { class: 'act' }, h('button', { class: 'btn sm dan', onclick: () => revoke(k) }, '폐기')))))))
    : h('div', { class: 'empty' }, '발급한 키가 없습니다'));
  const list = h('div', {}, table(r.items));
  return h('section', { class: 'panel' }, h('div', { class: 'hd' }, 'API 키'),
    h('div', { class: 'bd mu note' }, '터미널 스크립트 등에서 이 계정으로 쪽지를 보내거나 받을 때 씁니다: ',
      h('span', { class: 'mono' }, 'curl -H "Authorization: Bearer <키>" …/api/v1/send'),
      '. 받기 권한 키는 이 계정 앞으로 온 쪽지를 따로 받아 확인(ack)합니다.'),
    !r.allowed && h('div', { class: 'bd' }, h('div', { class: 'warnbox' }, '관리자가 API 키를 꺼 두어 지금은 키가 동작하지 않습니다.')),
    r.allowed && h('div', { class: 'bd inl' }, field('이름', name), h('label', {}, send, ' 보내기'), h('label', {}, recv, ' 받기'),
      h('button', { class: 'btn pri', onclick: create }, '발급')),
    h('div', { class: 'bd' }, shown),
    h('div', { class: 'bd flush' }, list));
}

function devicesPanel(devs, redraw) {
  async function act(dv, what) {
    const nm = dv.name || dv.id;
    const msg = what === 'kick'
      ? `${nm} 기기의 연결을 끊을까요?\n기기는 곧 다시 연결합니다. 막으려면 등록을 해제하세요.`
      : `${nm} 기기의 등록을 해제할까요?\n이 기기는 다시 비밀번호로 로그인해야 하며, 이 기기가 아직 받지 못한 쪽지는 삭제됩니다. 기기를 잃어버렸을 때 쓰세요.`;
    if (!confirm(msg)) return;
    try {
      await api(what === 'kick' ? 'POST' : 'DELETE', `/api/me/devices/${encodeURIComponent(dv.id)}${what === 'kick' ? '/kick' : ''}`);
      toast(what === 'kick' ? '연결을 끊었습니다' : '등록을 해제했습니다');
      redraw();
    } catch (e) { fail(e); }
  }
  const rows = devs.map((dv) => h('tr', {},
    h('td', {}, h('span', { class: 'dot' + (dv.online ? ' ok' : '') }), ' ', dv.name || h('span', { class: 'mu' }, '(이름 없음)')),
    h('td', {}, dv.disabled ? chip('er', '정지됨') : dv.online ? chip('ok', '접속 중') : chip('ne', '오프라인')),
    h('td', { class: 'mono' }, dv.id),
    h('td', {}, dv.host || ''),
    h('td', { class: 'mono' }, dv.app || ''),
    h('td', { class: 'mono' }, dv.last_ip || ''),
    h('td', {}, dv.online ? '지금' : ago(dv.last_seen)),
    h('td', { class: 'num' }, dv.pending ? num(dv.pending) : ''),
    h('td', { class: 'mono', title: '종단간 암호화 키 지문' }, dv.key || ''),
    h('td', { class: 'act' },
      dv.online && h('button', { class: 'btn sm', onclick: () => act(dv, 'kick') }, '연결 끊기'),
      h('button', { class: 'btn sm dan', onclick: () => act(dv, 'delete') }, '등록 해제'))));
  return h('section', { class: 'panel' }, h('div', { class: 'hd' }, `등록된 기기 ${devs.length}대`),
    h('div', { class: 'bd flush' }, devs.length
      ? h('div', { class: 'tw' }, h('table', { class: 'g' },
        h('thead', {}, h('tr', {}, ['기기', '상태', '기기 ID', '호스트', '앱', '마지막 IP', '마지막 접속', '받을 쪽지', '키 지문', ''].map((t) => h('th', {}, t)))),
        h('tbody', {}, rows)))
      : h('div', { class: 'empty' }, '등록된 기기가 없습니다')));
}

function passwordPanel(min) {
  const old = h('input', { type: 'password', autocomplete: 'current-password' });
  const n1 = h('input', { type: 'password', autocomplete: 'new-password' });
  const n2 = h('input', { type: 'password', autocomplete: 'new-password' });
  async function change() {
    if (n1.value !== n2.value) return fail('새 비밀번호가 서로 다릅니다');
    try {
      await api('POST', '/api/me/password', { old: old.value, new: n1.value });
      old.value = n1.value = n2.value = '';
      toast('비밀번호를 바꿨습니다. 다른 곳의 웹 로그인은 끝났습니다');
    } catch (e) { fail(e); }
  }
  return h('section', { class: 'panel' }, h('div', { class: 'hd' }, '비밀번호 변경'), h('div', { class: 'bd pw' },
    field('현재 비밀번호', old), field(`새 비밀번호 (${min}자 이상)`, n1), field('새 비밀번호 확인', n2),
    h('div', {}, h('button', { class: 'btn pri', onclick: change }, '변경')),
    h('div', { class: 'mu' }, '이미 등록된 기기는 비밀번호를 바꿔도 로그아웃되지 않습니다. 기기를 막으려면 위에서 등록을 해제하세요.')));
}

function activityPanel() {
  const box = h('div', { class: 'bd flush' });
  const pager = h('div', { class: 'pager' });
  let page = 1;
  const per = 50;
  async function load() {
    try {
      const r = await api('GET', `/api/me/activity?page=${page}&per=${per}`);
      box.replaceChildren(r.items.length
        ? h('div', { class: 'tw' }, h('table', { class: 'g' },
          h('thead', {}, h('tr', {}, ['시각', '작업', '주체', '내용'].map((t) => h('th', {}, t)))),
          h('tbody', {}, r.items.map((x) => h('tr', {},
            h('td', { class: 'mono' }, ts(x.ts)),
            h('td', {}, ACTIONS[x.action] || x.action),
            h('td', { class: 'mono' }, x.actor),
            h('td', { class: 'wrap' }, x.detail))))))
        : h('div', { class: 'empty' }, '기록이 없습니다'));
      const pages = Math.max(1, Math.ceil(r.total / per));
      pager.replaceChildren(
        h('button', { class: 'btn sm', disabled: page <= 1, onclick: () => { page--; load(); } }, '‹ 이전'),
        h('span', { class: 'mu' }, `${page} / ${pages} 쪽 · ${num(r.total)}건`),
        h('button', { class: 'btn sm', disabled: page >= pages, onclick: () => { page++; load(); } }, '다음 ›'));
    } catch (e) { fail(e); }
  }
  load();
  return h('section', { class: 'panel' }, h('div', { class: 'hd' }, '내 활동 기록', h('span', { class: 'sp' }), pager),
    h('div', { class: 'bd mu note' }, '가입, 기기 등록·이동·해제, 웹 로그인, 비밀번호 변경 등 이 계정과 기기에서 일어난 일입니다. 관리자가 한 작업은 표시되지 않습니다.'), box);
}

start();
