'use strict';
// PopupMessenger relay admin console. No framework: h() builds DOM nodes, every
// string goes in as a text node, so data from clients is never parsed as HTML.

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

// ---------------------------------------------------------------- formatting
const pad = (n) => String(n).padStart(2, '0');
function ts(t) {
  if (!t) return '—';
  const d = new Date(t);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
function tsFull(t) {
  if (!t) return '';
  const d = new Date(t);
  return `${ts(t)}:${pad(d.getSeconds())}`;
}
function dur(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}초`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}분`;
  const hr = Math.floor(m / 60);
  if (hr < 48) return `${hr}시간 ${m % 60}분`;
  return `${Math.floor(hr / 24)}일 ${hr % 24}시간`;
}
function ago(t) {
  if (!t) return h('span', { class: 'mu' }, '—');
  const ms = Date.now() - t;
  return h('span', { title: tsFull(t) }, ms < 0 ? `${dur(-ms)} 후` : ms < 60000 ? '방금' : `${dur(ms)} 전`);
}
function until(t) {
  if (!t) return '—';
  const ms = t - Date.now();
  return h('span', { title: tsFull(t), class: ms < 3600000 ? 'err' : '' }, ms <= 0 ? '만료됨' : `${dur(ms)} 남음`);
}
function bytes(n) {
  n = Number(n) || 0;
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(n < 10 ? 2 : 1)} ${u[i]}` : `${n} B`;
}
const num = (n) => (Number(n) || 0).toLocaleString('ko-KR');
const short = (id) => (id || '').slice(0, 8);

// ---------------------------------------------------------------- api
async function api(method, path, body) {
  const opt = { method, credentials: 'same-origin', headers: { 'X-Relay-Admin': '1' } };
  if (body !== undefined) {
    opt.headers['Content-Type'] = 'application/json';
    opt.body = JSON.stringify(body);
  }
  const r = await fetch(path, opt);
  let j = {};
  try { j = await r.json(); } catch { /* empty body */ }
  if (r.status === 401 && !['/api/login', '/api/setup'].includes(path)) {
    authScreen();
    throw new Error(j.error || '로그인이 필요합니다');
  }
  if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
  return j;
}

// ---------------------------------------------------------------- toast / modal / drawer
const toasts = h('div', { class: 'toasts' });
document.body.append(toasts);
function toast(msg, err) {
  const t = h('div', { class: 'toast' + (err ? ' er' : '') }, msg);
  toasts.append(t);
  setTimeout(() => t.remove(), err ? 6000 : 3000);
}
const fail = (e) => toast(String(e.message || e), true);

function modal(title, body, { ok = '확인', danger = false, cancel = '취소', onOk } = {}) {
  return new Promise((resolve) => {
    const err = h('div', { class: 'err' });
    const close = (v) => { scrim.remove(); box.remove(); document.removeEventListener('keydown', key); resolve(v); };
    const go = async () => {
      if (!onOk) return close(true);
      okBtn.disabled = true;
      try { const v = await onOk(); close(v === undefined ? true : v); } catch (e) { err.textContent = String(e.message || e); okBtn.disabled = false; }
    };
    const okBtn = h('button', { class: 'btn ' + (danger ? 'dan' : 'pri'), onclick: go }, ok);
    const key = (e) => { if (e.key === 'Escape') close(false); if (e.key === 'Enter' && e.target.tagName !== 'TEXTAREA') go(); };
    const scrim = h('div', { class: 'scrim', onclick: () => close(false) });
    const box = h('div', { class: 'modal', role: 'dialog' },
      h('div', { class: 'hd' }, title),
      h('div', { class: 'bd' }, body, err),
      h('div', { class: 'ft' }, cancel && h('button', { class: 'btn', onclick: () => close(false) }, cancel), okBtn));
    document.addEventListener('keydown', key);
    document.body.append(scrim, box);
    (box.querySelector('input,select,textarea') || okBtn).focus();
  });
}
const confirmBox = (title, lines, ok = '확인', danger = false) =>
  modal(title, h('div', {}, [].concat(lines).map((l) => (l instanceof Node ? l : h('div', {}, l)))), { ok, danger });

function drawer(title, body, foot) {
  const close = () => { scrim.remove(); d.remove(); document.removeEventListener('keydown', key); };
  const key = (e) => { if (e.key === 'Escape' && !document.querySelector('.modal')) close(); };
  const scrim = h('div', { class: 'scrim', onclick: close });
  const d = h('div', { class: 'drawer' },
    h('div', { class: 'hd' }, h('h2', {}, title), h('span', { class: 'sp' }), h('button', { class: 'btn', onclick: close }, '닫기')),
    h('div', { class: 'bd' }, body),
    foot && h('div', { class: 'ft' }, foot));
  document.addEventListener('keydown', key);
  document.body.append(scrim, d);
  return { close, el: d };
}

const field = (label, input) => h('div', { class: 'fld' }, h('label', { class: 'mu' }, label), input);

// ---------------------------------------------------------------- data grid
const sortState = {};
// cols: { k, t, v(row) -> node|string, s(row) -> sort value, cls, sort: false }
function grid(id, cols, rows, { key = 'id', sel, onSel, onRow, empty = '항목이 없습니다' } = {}) {
  const st = sortState[id] || (sortState[id] = { k: null, dir: 1 });
  const wrap = h('div', { class: 'tw' });
  function draw() {
    let list = rows.slice();
    const col = cols.find((c) => c.k === st.k);
    if (col) {
      const sv = col.s || ((r) => r[col.k]);
      list.sort((a, b) => {
        const x = sv(a), y = sv(b);
        if (x == null && y == null) return 0;
        if (x == null) return 1;
        if (y == null) return -1;
        return (typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), 'ko')) * st.dir;
      });
    }
    const head = h('tr', {},
      sel && h('th', { class: 'cb' }, h('input', {
        type: 'checkbox',
        checked: list.length > 0 && list.every((r) => sel.has(r[key])),
        onchange: (e) => { list.forEach((r) => (e.target.checked ? sel.add(r[key]) : sel.delete(r[key]))); onSel && onSel(); draw(); },
      })),
      cols.map((c) => h('th', {
        class: [c.cls, c.sort !== false ? 's' : ''].filter(Boolean).join(' '),
        onclick: c.sort === false ? null : () => { st.dir = st.k === c.k ? -st.dir : 1; st.k = c.k; draw(); },
      }, c.t, st.k === c.k && h('span', { class: 'ar' }, st.dir > 0 ? '▲' : '▼'))));
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

const chip = (cls, text, title) => h('span', { class: 'chip ' + cls, title }, text);
const STATE = { active: ['ok', '활성'], pending: ['wa', '승인 대기'], disabled: ['er', '정지'] };
const stateChip = (s) => chip(...(STATE[s] || ['ne', s]));
const STATUS = { online: ['ok', '온라인'], dnd: ['wa', '방해금지'], offline: ['ne', '오프라인'] };
const ENC = { plain: '평문', tls: '서버 통신 암호화 (TLS)', e2ee: '종단간 암호화' };
const REG = { closed: '닫힘', approval: '승인제', open: '열림' };
const AUTH = { password: '사용자 이름 + 비밀번호', none: '없음 (기기 인증)' };

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
let timer = null;

async function authScreen() {
  clearInterval(timer);
  ME = null;
  const s = await fetch('/api/session', { credentials: 'same-origin' }).then((r) => r.json());
  if (s.user) { ME = s.user; return shell(); }
  const user = h('input', { type: 'text', autocomplete: 'username', placeholder: 'admin' });
  const pass = h('input', { type: 'password', autocomplete: s.setup ? 'new-password' : 'current-password' });
  const pass2 = h('input', { type: 'password', autocomplete: 'new-password' });
  const err = h('div', { class: 'err' });
  const btn = h('button', { class: 'btn pri' }, s.setup ? '관리자 계정 만들기' : '로그인');
  async function go(e) {
    e.preventDefault();
    err.textContent = '';
    if (s.setup && pass.value !== pass2.value) { err.textContent = '비밀번호가 서로 다릅니다'; return; }
    btn.disabled = true;
    try {
      const r = await api('POST', s.setup ? '/api/setup' : '/api/login', { username: user.value.trim(), password: pass.value });
      ME = r.user;
      shell();
    } catch (x) { err.textContent = String(x.message || x); btn.disabled = false; }
  }
  $app.replaceChildren(h('div', { class: 'center' }, h('form', { class: 'card', onsubmit: go },
    h('div', { class: 'mu' }, 'PopupMessenger Relay'),
    h('h1', {}, s.setup ? '최초 설정' : s.server_name),
    s.setup && h('div', { class: 'warnbox' }, '아직 관리자 계정이 없습니다. 지금 만드는 계정이 첫 관리자가 됩니다. 이 화면은 첫 계정을 만든 뒤 다시 나타나지 않습니다.'),
    field('관리자 이름', user),
    field(s.setup ? `비밀번호 (${Math.max(8, s.min_password_len)}자 이상)` : '비밀번호', pass),
    s.setup && field('비밀번호 확인', pass2),
    err, btn)));
  user.focus();
}

// ---------------------------------------------------------------- shell & routing
const PAGES = [
  ['운영'],
  ['overview', '개요', pOverview],
  ['connections', '접속 현황', pConnections],
  ['사용자'],
  ['accounts', '계정', pAccounts],
  ['devices', '기기', pDevices],
  ['데이터'],
  ['messages', '보관함', pMessages],
  ['archive', '쪽지 감사', pArchive],
  ['audit', '감사 로그', pAudit],
  ['설정'],
  ['policy', '정책', pPolicy],
  ['admins', '관리자', pAdmins],
];
const navCounts = {};
let nav, main;

function shell() {
  const srv = h('span', { class: 'srv' });
  nav = h('nav', { class: 'side' });
  main = h('main', { class: 'main' });
  $app.replaceChildren(h('div', { class: 'shell' },
    h('header', { class: 'top' },
      h('span', { class: 'brand' }, 'Relay Admin'), srv, h('span', { class: 'sp' }),
      h('span', { class: 'who' }, ME),
      h('button', { onclick: async () => { try { await api('POST', '/api/logout'); } catch { /* ignore */ } authScreen(); } }, '로그아웃')),
    nav, main));
  fetch('/api/session', { credentials: 'same-origin' }).then((r) => r.json()).then((s) => { srv.textContent = s.server_name; });
  route();
  refreshCounts();
}

function drawNav(cur) {
  nav.replaceChildren(...PAGES.map((p) => (p.length === 1
    ? h('div', { class: 'grp' }, p[0])
    : h('a', { href: `#/${p[0]}`, class: p[0] === cur ? 'on' : '' }, p[1],
      navCounts[p[0]] ? h('span', { class: 'cnt' + (p[0] === 'accounts' ? ' wa' : '') }, navCounts[p[0]]) : null))));
}

async function refreshCounts() {
  try {
    const o = await api('GET', '/api/overview');
    navCounts.accounts = o.counts.pending || 0;
    navCounts.connections = o.counts.online || 0;
    drawNav(current());
  } catch { /* shown elsewhere */ }
}

const current = () => (location.hash.replace(/^#\//, '') || 'overview').split('?')[0];

async function route() {
  if (!ME) return;
  clearInterval(timer);
  const id = current();
  const page = PAGES.find((p) => p[0] === id) || PAGES[1];
  drawNav(page[0]);
  const ph = h('div', { class: 'ph' }, h('h1', {}, page[1]));
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

// ---------------------------------------------------------------- overview
async function pOverview(c, ph) {
  ph.append(h('span', { class: 'sp' }), h('span', { class: 'mu' }, '5초마다 갱신'));
  async function draw() {
    const o = await api('GET', '/api/overview');
    const k = o.counts, s = o.server, p = o.policy;
    navCounts.accounts = k.pending; navCounts.connections = k.online;
    drawNav('overview');
    const kpi = (label, v, d, warn) => h('div', { class: 'kpi' + (warn ? ' wa' : '') }, h('div', { class: 'k' }, label), h('div', { class: 'v' }, v), d && h('div', { class: 'd' }, d));
    const maxMsgs = Math.max(1, ...o.stats.map((x) => x.msgs));
    c.replaceChildren(
      h('div', { class: 'kpis' },
        kpi('접속 중인 기기', num(k.online), k.online_plain ? `평문 연결 ${k.online_plain}` : 'TLS 연결만', false),
        kpi('활성 계정', num(k.active), `전체 ${num(k.accounts)} · 정지 ${num(k.disabled)}`),
        kpi('승인 대기', num(k.pending), p.registration === 'approval' ? '가입 승인제' : `가입: ${REG[p.registration]}`, k.pending > 0),
        kpi('등록 기기', num(k.devices), `정지 ${num(k.devices_disabled)}`),
        kpi('보관 중인 쪽지', num(k.messages), k.oldest_message ? h('span', {}, '가장 오래된 것 ', ago(k.oldest_message)) : '—'),
        kpi('전달 대기', num(k.pending_deliveries), '기기 단위'),
        kpi('첨부 보관 용량', bytes(k.stored_bytes)),
        kpi('만료 알림 대기', num(k.expired_notices), '아직 로그온하지 않은 기기'),
        kpi('감사 보관', num(k.archived), '평문 쪽지 사본'),
        kpi('DB 크기', bytes(s.db_bytes))),
      h('div', { class: 'cols' },
        h('div', { class: 'panel' }, h('div', { class: 'hd' }, '서버'), h('div', { class: 'bd' }, h('div', { class: 'kv' },
          h('div', {}, '서버 이름'), h('div', {}, s.name),
          h('div', {}, '서버 ID'), h('div', { class: 'mono' }, s.id),
          h('div', {}, '버전'), h('div', {}, s.version),
          h('div', {}, '가동 시간'), h('div', { title: tsFull(s.started) }, dur(s.now - s.started)),
          h('div', {}, '릴레이 포트'), h('div', {}, `${s.relay_port} (TCP · TLS 1.3 / 평문 자동 판별)`),
          h('div', {}, '인증서'), h('div', {}, s.tls_self_signed ? chip('wa', '자체 서명') : chip('ok', '운영자 제공')),
          h('div', {}, 'SHA-256 지문'), h('div', { class: 'mono' }, s.tls_fingerprint.match(/../g).join(':'), ' ',
            h('a', { onclick: () => navigator.clipboard.writeText(s.tls_fingerprint).then(() => toast('복사했습니다')) }, '복사')),
          h('div', {}, '데이터 경로'), h('div', { class: 'mono' }, s.data_dir)))),
        h('div', { class: 'panel' }, h('div', { class: 'hd' }, '현재 정책', h('span', { class: 'sp' }), h('a', { href: '#/policy' }, '변경')), h('div', { class: 'bd' }, h('div', { class: 'kv' },
          h('div', {}, '로그온'), h('div', {}, AUTH[p.auth]),
          h('div', {}, '외부인 가입'), h('div', {}, REG[p.registration]),
          h('div', {}, '계정당 기기'), h('div', {}, `${p.max_devices}대`),
          h('div', {}, '암호화'), h('div', {}, ENC[p.encryption]),
          h('div', {}, '경유 강제'), h('div', {}, p.force_relay ? (p.block_direct_outsiders ? '예 · 외부 LAN 직접 전송 금지' : '예') : '아니요'),
          h('div', {}, '첨부 한도'), h('div', {}, p.relay_max_files ? `${p.relay_max_files}개 · 파일당 ${bytes(p.relay_max_file_size)}` : '첨부 중계 안 함'),
          h('div', {}, '보관'), h('div', {}, `쪽지 ${p.retention_hours.text}시간 · 첨부 ${p.retention_hours.files}시간`),
          h('div', {}, '연락처'), h('div', {}, p.contacts === 'all' ? '전체 공개' : '검색한 사용자만'))))),
      h('div', { class: 'panel' }, h('div', { class: 'hd' }, '일별 처리량 (UTC, 최근 30일)'), h('div', { class: 'bd flush' },
        grid('stats', [
          { k: 'day', t: '날짜', cls: 'mono' },
          { k: 'msgs', t: '쪽지', cls: 'num', v: (r) => num(r.msgs) },
          { k: 'bar', t: '', sort: false, v: (r) => { const b = h('div', { class: 'bar' }); b.style.width = `${(r.msgs / maxMsgs) * 100}%`; return h('div', { class: 'bartrack' }, b); } },
          { k: 'deliveries', t: '기기 전달', cls: 'num', v: (r) => num(r.deliveries) },
          { k: 'bytes', t: '첨부 용량', cls: 'num', v: (r) => bytes(r.bytes) },
          { k: 'expired', t: '만료 (기기 단위)', cls: 'num', v: (r) => num(r.expired) },
        ], o.stats, { key: 'day', empty: '아직 기록이 없습니다' }))));
  }
  await draw();
  every(5000, () => draw().catch(fail));
}

// ---------------------------------------------------------------- connections
async function pConnections(c, ph) {
  const q = h('input', { type: 'search', placeholder: '이름, 계정, 기기 ID, IP', oninput: () => draw() });
  ph.append(h('span', { class: 'sp' }), h('span', { class: 'mu' }, '3초마다 갱신'));
  let rows = [];
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  async function load() { rows = (await api('GET', '/api/connections')).connections; draw(); }
  function draw() {
    const s = q.value.trim().toLowerCase();
    const list = rows.filter((r) => !s || `${r.name} ${r.username || ''} ${r.device} ${r.account} ${r.ip}`.toLowerCase().includes(s));
    host.replaceChildren(grid('conns', [
      { k: 'name', t: '이름', v: (r) => r.name || h('span', { class: 'mu' }, '(이름 없음)') },
      { k: 'username', t: '사용자 이름', v: (r) => r.username || h('span', { class: 'mu' }, '—') },
      { k: 'status', t: '상태', v: (r) => chip(...(STATUS[r.status] || STATUS.online)) },
      { k: 'device', t: '기기 ID', cls: 'mono' },
      { k: 'account', t: '계정 ID', cls: 'mono' },
      { k: 'ip', t: 'IP', cls: 'mono' },
      { k: 'tls', t: '연결', v: (r) => (r.tls ? chip('ok', 'TLS') : chip('wa', '평문')) },
      { k: 'app', t: '앱', cls: 'mono' },
      { k: 'since', t: '접속 시각', v: (r) => h('span', { title: tsFull(r.since) }, ts(r.since)) },
      { k: 'dur', t: '접속 시간', cls: 'num', s: (r) => -r.since, v: (r) => dur(Date.now() - r.since) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (r) => h('button', { class: 'btn sm', onclick: () => kick(r.device, r.name) }, '끊기') },
    ], list, { key: 'device', empty: '접속 중인 기기가 없습니다' }));
    foot.textContent = `${list.length}개 표시 · 전체 ${rows.length}개 · 평문 ${rows.filter((r) => !r.tls).length}개`;
  }
  async function kick(id, name) {
    if (!(await confirmBox('연결 끊기', [`${name || id} 기기의 연결을 끊을까요?`, '클라이언트는 곧바로 다시 접속할 수 있습니다. 막으려면 기기를 정지하세요.'], '끊기', true))) return;
    try { await api('POST', `/api/devices/${encodeURIComponent(id)}/kick`); toast('연결을 끊었습니다'); load(); } catch (e) { fail(e); }
  }
  c.append(h('div', { class: 'tb' }, q), host, foot);
  await load();
  every(3000, () => load().catch(fail));
}

// ---------------------------------------------------------------- accounts
async function pAccounts(c, ph) {
  const [{ accounts }, { settings }] = await Promise.all([api('GET', '/api/accounts'), api('GET', '/api/settings')]);
  let rows = accounts;
  const pol = settings.policy;
  let filter = new URLSearchParams(location.hash.split('?')[1] || '').get('state') || (rows.some((r) => r.state === 'pending') ? 'pending' : 'all');
  const sel = new Set();
  const q = h('input', { type: 'search', placeholder: '사용자 이름, 표시 이름, ID, 메모', oninput: () => draw() });
  const seg = h('div', { class: 'seg' });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  const bulkBtns = [
    ['approve', '승인', false], ['enable', '정지 해제', false], ['disable', '정지', true], ['delete', '삭제', true],
  ].map(([a, t, d]) => h('button', { class: 'btn' + (d ? ' dan' : ''), disabled: true, onclick: () => bulk(a, t) }, t));
  const selInfo = h('span', { class: 'mu' });
  ph.append(h('span', { class: 'sub' }, pol.auth === 'none' ? '로그온 방식이 "없음"이라 기기 하나가 계정 하나입니다' : ''), h('span', { class: 'sp' }),
    h('button', { class: 'btn pri', onclick: create }, '새 계정'));

  async function reload() { rows = (await api('GET', '/api/accounts')).accounts; sel.forEach((id) => { if (!rows.some((r) => r.id === id)) sel.delete(id); }); draw(); refreshCounts(); }
  function onSel() {
    bulkBtns.forEach((b) => { b.disabled = !sel.size; });
    selInfo.textContent = sel.size ? `${sel.size}개 선택` : '';
  }
  function draw() {
    const n = (s) => rows.filter((r) => r.state === s).length;
    seg.replaceChildren(...[['all', `전체 ${rows.length}`], ['active', `활성 ${n('active')}`], ['pending', `승인 대기 ${n('pending')}`], ['disabled', `정지 ${n('disabled')}`]]
      .map(([k, t]) => h('button', { class: filter === k ? 'on' : '', onclick: () => { filter = k; draw(); } }, t)));
    const s = q.value.trim().toLowerCase();
    const list = rows.filter((r) => (filter === 'all' || r.state === filter) && (!s || `${r.username || ''} ${r.name} ${r.id} ${r.note}`.toLowerCase().includes(s)));
    host.replaceChildren(grid('accounts', [
      { k: 'state', t: '상태', v: (r) => stateChip(r.state) },
      { k: 'username', t: '사용자 이름', v: (r) => r.username || h('span', { class: 'mu' }, '—') },
      { k: 'name', t: '표시 이름' },
      { k: 'id', t: '계정 ID', cls: 'mono' },
      { k: 'devices', t: '기기', cls: 'num', v: (r) => `${r.devices} / ${r.max_devices ?? pol.max_devices}${r.max_devices != null ? '*' : ''}` },
      { k: 'online', t: '접속', cls: 'num', v: (r) => (r.online ? h('span', {}, h('span', { class: 'dot ok' }), ` ${r.online}`) : h('span', { class: 'mu' }, '0')) },
      { k: 'pending', t: '받을 쪽지', cls: 'num', v: (r) => (r.pending ? num(r.pending) : h('span', { class: 'mu' }, '0')) },
      { k: 'stored_bytes', t: '보낸 첨부 보관', cls: 'num', v: (r) => (r.stored_bytes ? bytes(r.stored_bytes) : h('span', { class: 'mu' }, '—')) },
      { k: 'quota_mb', t: '저장 한도', cls: 'num', v: (r) => (r.quota_mb != null ? `${num(r.quota_mb)} MB*` : h('span', { class: 'mu' }, settings.account_quota_mb ? `${num(settings.account_quota_mb)} MB` : '무제한')) },
      { k: 'last_seen', t: '마지막 접속', v: (r) => ago(r.last_seen) },
      { k: 'created', t: '생성', v: (r) => h('span', { title: tsFull(r.created) }, ts(r.created)) },
      { k: 'note', t: '메모', cls: 'trunc', v: (r) => h('span', { title: r.note }, r.note) },
    ], list, { sel, onSel, onRow: open, empty: filter === 'pending' ? '승인을 기다리는 가입 신청이 없습니다' : '계정이 없습니다' }));
    foot.replaceChildren(`${list.length}개 표시 · 전체 ${rows.length}개`, h('span', {}, '* 계정별로 따로 정한 값'));
    onSel();
  }

  async function bulk(action, label) {
    const ids = [...sel];
    const names = rows.filter((r) => sel.has(r.id)).map((r) => r.username || r.name || r.id);
    const lines = [`선택한 ${ids.length}개 계정을 ${label}할까요?`, h('div', { class: 'mu' }, names.slice(0, 12).join(', ') + (names.length > 12 ? ` 외 ${names.length - 12}개` : ''))];
    if (action === 'delete') lines.push(h('div', { class: 'warnbox' }, '계정과 등록된 기기, 받지 못한 쪽지가 모두 삭제됩니다. 되돌릴 수 없습니다.'));
    if (action === 'disable') lines.push('접속 중인 기기는 곧바로 연결이 끊깁니다.');
    if (!(await confirmBox(`계정 ${label}`, lines, label, action === 'delete' || action === 'disable'))) return;
    try {
      const r = await api('POST', '/api/accounts/bulk', { ids, action });
      toast(`${r.count}개 계정을 ${label}했습니다`);
      sel.clear();
      reload();
    } catch (e) { fail(e); }
  }

  async function create() {
    const f = {
      username: h('input', { type: 'text', autocomplete: 'off' }),
      password: h('input', { type: 'password', autocomplete: 'new-password' }),
      name: h('input', { type: 'text', placeholder: '비우면 사용자 이름' }),
      max_devices: h('input', { type: 'number', min: 1, max: 9999, placeholder: `정책 기본값 (${pol.max_devices})` }),
      quota_mb: h('input', { type: 'number', min: 0, placeholder: '정책 기본값' }),
      note: h('input', { type: 'text' }),
    };
    const ok = await modal('새 계정', [
      pol.auth === 'none' && h('div', { class: 'warnbox' }, '지금 로그온 방식은 "없음"입니다. 사용자 이름으로 만든 계정은 로그온 방식이 "비밀번호"일 때 쓸 수 있습니다.'),
      field('사용자 이름', f.username), field(`비밀번호 (${settings.min_password_len}자 이상)`, f.password), field('표시 이름', f.name),
      field('기기 수 한도', f.max_devices), field('저장 한도 (MB)', f.quota_mb), field('메모', f.note),
    ], {
      ok: '만들기',
      onOk: () => api('POST', '/api/accounts', {
        username: f.username.value.trim(), password: f.password.value, name: f.name.value.trim(), note: f.note.value,
        max_devices: f.max_devices.value === '' ? null : Number(f.max_devices.value),
        quota_mb: f.quota_mb.value === '' ? null : Number(f.quota_mb.value),
      }),
    });
    if (ok) { toast('계정을 만들었습니다'); reload(); }
  }

  async function open(a) {
    const { devices } = await api('GET', '/api/devices');
    const devs = devices.filter((d) => d.account === a.id);
    const f = {
      state: h('select', {}, Object.entries(STATE).map(([k, [, t]]) => h('option', { value: k, selected: a.state === k }, t))),
      username: h('input', { type: 'text', value: a.username || '', placeholder: '없음' }),
      name: h('input', { type: 'text', value: a.name }),
      password: h('input', { type: 'password', autocomplete: 'new-password', placeholder: '바꿀 때만 입력' }),
      max_devices: h('input', { type: 'number', min: 1, max: 9999, value: a.max_devices ?? '', placeholder: `정책 기본값 (${pol.max_devices})` }),
      quota_mb: h('input', { type: 'number', min: 0, value: a.quota_mb ?? '', placeholder: settings.account_quota_mb ? `정책 기본값 (${settings.account_quota_mb})` : '정책 기본값 (무제한)' }),
      note: h('textarea', { rows: 3, value: a.note }),
    };
    const row = (label, ctl, help) => h('div', { class: 'row' }, h('label', {}, label), h('div', { class: 'ctl' }, ctl, help && h('div', { class: 'help' }, help)));
    const devGrid = grid(`acc-devs`, [
      { k: 'online', t: '', v: (d) => h('span', { class: 'dot' + (d.online ? ' ok' : '') }), s: (d) => (d.online ? 0 : 1) },
      { k: 'name', t: '이름', s: (d) => d.profile.name, v: (d) => d.profile.name || '—' },
      { k: 'id', t: '기기 ID', cls: 'mono' },
      { k: 'app', t: '앱', cls: 'mono' },
      { k: 'last_seen', t: '마지막 접속', v: (d) => ago(d.last_seen) },
      { k: 'disabled', t: '', v: (d) => (d.disabled ? chip('er', '정지') : '') },
    ], devs, { empty: '등록된 기기가 없습니다' });
    const dr = drawer(a.username || a.name || a.id, [
      h('div', { class: 'kv' },
        h('div', {}, '계정 ID'), h('div', { class: 'mono' }, a.id),
        h('div', {}, '생성'), h('div', {}, tsFull(a.created)),
        h('div', {}, '마지막 접속'), h('div', {}, a.last_seen ? tsFull(a.last_seen) : '—'),
        h('div', {}, '받을 쪽지'), h('div', {}, `${num(a.pending)}건 (기기 단위)`),
        h('div', {}, '비밀번호'), h('div', {}, a.has_password ? '설정됨' : '없음')),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, '계정 정보'),
        row('상태', f.state, '정지하거나 승인 대기로 돌리면 접속 중인 기기의 연결이 끊깁니다'),
        row('사용자 이름', f.username, '비우면 비밀번호 로그온을 쓸 수 없습니다'),
        row('표시 이름', f.name, '기기가 접속하면 기기에서 보낸 이름으로 바뀝니다'),
        row('새 비밀번호', f.password),
        row('기기 수 한도', f.max_devices, `비우면 정책 기본값. 1~9999`),
        row('저장 한도 (MB)', f.quota_mb, '이 계정이 보낸 첨부파일의 서버 보관 용량. 비우면 정책 기본값, 0은 무제한'),
        row('메모', f.note)),
      h('div', { class: 'panel' }, h('div', { class: 'hd' }, `기기 ${devs.length}대`, h('span', { class: 'sp' }), h('a', { href: '#/devices', onclick: () => dr.close() }, '기기 관리')), h('div', { class: 'bd flush' }, devGrid)),
    ], [
      h('button', { class: 'btn dan', onclick: del }, '계정 삭제'), h('span', { class: 'sp' }),
      h('button', { class: 'btn', onclick: () => dr.close() }, '취소'),
      h('button', { class: 'btn pri', onclick: save }, '저장'),
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
      if (!Object.keys(p).length) return dr.close();
      try { await api('PATCH', `/api/accounts/${a.id}`, p); toast('저장했습니다'); dr.close(); reload(); } catch (e) { fail(e); }
    }
    async function del() {
      if (!(await confirmBox('계정 삭제', [`${a.username || a.name || a.id} 계정을 삭제할까요?`, h('div', { class: 'warnbox' }, `등록된 기기 ${devs.length}대와 받지 못한 쪽지 ${a.pending}건이 함께 삭제됩니다. 되돌릴 수 없습니다.`)], '삭제', true))) return;
      try { await api('DELETE', `/api/accounts/${a.id}`); toast('삭제했습니다'); dr.close(); reload(); } catch (e) { fail(e); }
    }
  }

  c.append(h('div', { class: 'tb' }, seg, q, h('span', { class: 'sp' }), selInfo, bulkBtns), host, foot);
  draw();
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('id');
  if (want) { const a = rows.find((r) => r.id === want); if (a) open(a); }
}

// ---------------------------------------------------------------- devices
async function pDevices(c) {
  let rows = (await api('GET', '/api/devices')).devices;
  let filter = 'all';
  const q = h('input', { type: 'search', placeholder: '이름, 계정, 기기 ID, IP, 호스트, 키 지문', oninput: () => draw() });
  const seg = h('div', { class: 'seg' });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  async function reload() { rows = (await api('GET', '/api/devices')).devices; draw(); }
  function draw() {
    seg.replaceChildren(...[['all', `전체 ${rows.length}`], ['online', `접속 중 ${rows.filter((r) => r.online).length}`], ['disabled', `정지 ${rows.filter((r) => r.disabled).length}`], ['waiting', `받을 쪽지 있음 ${rows.filter((r) => r.pending).length}`]]
      .map(([k, t]) => h('button', { class: filter === k ? 'on' : '', onclick: () => { filter = k; draw(); } }, t)));
    const s = q.value.trim().toLowerCase();
    const list = rows.filter((r) => (filter === 'all' || (filter === 'online' && r.online) || (filter === 'disabled' && r.disabled) || (filter === 'waiting' && r.pending))
      && (!s || `${r.profile.name || ''} ${r.username || ''} ${r.account_name} ${r.id} ${r.account} ${r.last_ip} ${r.profile.host || ''} ${r.key}`.toLowerCase().includes(s)));
    host.replaceChildren(grid('devices', [
      { k: 'online', t: '', s: (r) => (r.online ? 0 : 1), v: (r) => h('span', { class: 'dot' + (r.online ? ' ok' : ''), title: r.online ? '접속 중' : '오프라인' }) },
      { k: 'name', t: '기기 이름', s: (r) => r.profile.name, v: (r) => r.profile.name || h('span', { class: 'mu' }, '—') },
      { k: 'account', t: '계정', s: (r) => r.username || r.account_name, v: (r) => h('a', { href: `#/accounts?state=all&id=${r.account}` }, r.username || r.account_name || short(r.account)) },
      { k: 'account_state', t: '계정 상태', v: (r) => stateChip(r.account_state) },
      { k: 'status', t: '상태 설정', s: (r) => r.profile.status, v: (r) => (r.profile.status ? chip(...(STATUS[r.profile.status] || STATUS.online)) : '') },
      { k: 'host', t: '호스트', s: (r) => r.profile.host, v: (r) => r.profile.host || '' },
      { k: 'id', t: '기기 ID', cls: 'mono' },
      { k: 'app', t: '앱', cls: 'mono' },
      { k: 'last_ip', t: '마지막 IP', cls: 'mono' },
      { k: 'key', t: 'E2EE 키 지문', cls: 'mono', v: (r) => r.key || h('span', { class: 'mu' }, '없음') },
      { k: 'limits', t: '수신 한도', cls: 'num', sort: false, v: (r) => `${r.profile.max_files || '∞'}개 · ${r.profile.max_file_size ? bytes(r.profile.max_file_size) : '∞'}` },
      { k: 'pending', t: '받을 쪽지', cls: 'num', v: (r) => (r.pending ? num(r.pending) : h('span', { class: 'mu' }, '0')) },
      { k: 'last_seen', t: '마지막 접속', v: (r) => ago(r.last_seen) },
      { k: 'created', t: '등록', v: (r) => h('span', { title: tsFull(r.created) }, ts(r.created)) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (r) => [
        r.online && h('button', { class: 'btn sm', onclick: () => act(r, 'kick') }, '끊기'),
        h('button', { class: 'btn sm', onclick: () => act(r, r.disabled ? 'enable' : 'disable') }, r.disabled ? '정지 해제' : '정지'),
        h('button', { class: 'btn sm dan', onclick: () => act(r, 'delete') }, '등록 해제'),
      ] },
    ], list, { empty: '기기가 없습니다' }));
    foot.textContent = `${list.length}개 표시 · 전체 ${rows.length}개`;
  }
  async function act(d, what) {
    const name = d.profile.name || d.id;
    const T = {
      kick: ['연결 끊기', [`${name} 기기의 연결을 끊을까요?`], '끊기'],
      disable: ['기기 정지', [`${name} 기기를 정지할까요?`, '정지된 기기는 로그온할 수 없고, 연락처에서도 빠집니다.'], '정지'],
      enable: ['기기 정지 해제', [`${name} 기기의 정지를 풀까요?`], '정지 해제'],
      delete: ['기기 등록 해제', [`${name} 기기의 등록을 해제할까요?`, h('div', { class: 'warnbox' }, `이 기기가 받지 못한 쪽지 ${d.pending}건이 삭제됩니다. 기기는 다음 로그온 때 새 기기로 다시 등록됩니다 (기기 수 한도 적용).`)], '등록 해제'],
    }[what];
    if (!(await confirmBox(T[0], T[1], T[2], what !== 'enable'))) return;
    try {
      if (what === 'kick') await api('POST', `/api/devices/${encodeURIComponent(d.id)}/kick`);
      else if (what === 'delete') await api('DELETE', `/api/devices/${encodeURIComponent(d.id)}`);
      else await api('PATCH', `/api/devices/${encodeURIComponent(d.id)}`, { disabled: what === 'disable' });
      toast('처리했습니다');
      reload();
    } catch (e) { fail(e); }
  }
  c.append(h('div', { class: 'tb' }, seg, q), host, foot);
  draw();
}

// ---------------------------------------------------------------- stored messages
async function pMessages(c, ph) {
  let rows = (await api('GET', '/api/messages')).messages;
  const q = h('input', { type: 'search', placeholder: '보낸 사람, 받는 사람, ID', oninput: () => draw() });
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  ph.append(h('span', { class: 'sub' }, '평문 쪽지는 행을 눌러 내용과 첨부파일을 열람할 수 있습니다. 열람 기록은 감사 로그에 남습니다'), h('span', { class: 'sp' }),
    h('button', { class: 'btn', onclick: sweep }, '만료 처리 지금 실행'));
  async function reload() { rows = (await api('GET', '/api/messages')).messages; draw(); }
  function draw() {
    const s = q.value.trim().toLowerCase();
    const list = rows.filter((r) => !s || `${r.from_name} ${r.to || ''} ${r.mid} ${r.from}`.toLowerCase().includes(s));
    host.replaceChildren(grid('messages', [
      { k: 'created', t: '보관 시작', v: (r) => h('span', { title: tsFull(r.created) }, ts(r.created)) },
      { k: 'from_name', t: '보낸 사람', v: (r) => h('a', { href: `#/accounts?state=all&id=${r.from}` }, r.from_name || short(r.from)) },
      { k: 'to', t: '받는 사람', cls: 'trunc', v: (r) => h('span', { title: r.to || '' }, r.to || '—') },
      { k: 'pending', t: '전달 대기', cls: 'num', v: (r) => `${r.pending} / ${r.deliveries} 기기` },
      { k: 'files', t: '첨부', cls: 'num', v: (r) => (r.files ? (r.files_gone ? h('span', { class: 'mu' }, `${r.files}개 (삭제됨)`) : `${r.files}개`) : h('span', { class: 'mu' }, '—')) },
      { k: 'bytes', t: '크기', cls: 'num', v: (r) => (r.bytes ? bytes(r.bytes) : h('span', { class: 'mu' }, '—')) },
      { k: 'enc', t: '본문', v: (r) => (r.enc === 'none' ? chip('ne', '평문') : chip('ok', '암호화')) },
      { k: 'files_expires', t: '첨부 만료', v: (r) => (!r.files || r.files_gone ? h('span', { class: 'mu' }, '—') : until(r.files_expires)) },
      { k: 'text_expires', t: '쪽지 만료', v: (r) => until(r.text_expires) },
      { k: 'mid', t: 'ID', cls: 'mono' },
      { k: 'act', t: '', sort: false, cls: 'act', v: (r) => h('button', { class: 'btn sm dan', onclick: () => purge(r) }, '삭제') },
    ], list, { key: 'mid', onRow: (r) => openMessage(r.mid, reload), empty: '보관 중인 쪽지가 없습니다' }));
    const total = list.reduce((a, r) => a + (r.files_gone ? 0 : r.bytes), 0);
    foot.textContent = `${list.length}건 표시 · 첨부 ${bytes(total)} · 전달 대기 ${num(list.reduce((a, r) => a + r.pending, 0))}건 (기기 단위)`;
  }
  async function purge(m) {
    if (!(await confirmBox('보관 쪽지 삭제', [`${m.from_name}님이 보낸 쪽지를 지금 삭제할까요?`, `아직 받지 못한 기기 ${m.pending}대는 이 쪽지를 받을 수 없게 되며, 만료 알림도 남지 않습니다.`], '삭제', true))) return;
    try { await api('DELETE', `/api/messages/${m.mid}`); toast('삭제했습니다'); reload(); } catch (e) { fail(e); }
  }
  async function sweep() {
    try {
      const r = await api('POST', '/api/sweep');
      toast(`첨부 ${r.files}건 · 쪽지 ${r.messages}건 만료 처리 (만료 알림 ${r.notices}건)`);
      reload();
    } catch (e) { fail(e); }
  }
  c.append(h('div', { class: 'tb' }, q), host, foot);
  draw();
}

// Shared pieces of the message viewers.
const msgBody = (text) => h('div', { class: 'msgbody' }, text || h('span', { class: 'mu' }, '(본문 없음)'));
function groupBox(g) {
  if (!g || !g.name) return null;
  return h('div', { class: 'kv' },
    h('div', {}, '그룹'), h('div', {}, g.name),
    h('div', {}, '그룹 구성원'), h('div', {}, (g.members || []).map((m) => m.name || m.id).join(', ') || '—'));
}
function fileTable(id, files, href, kept) {
  return grid(id, [
    { k: 'idx', t: '#', cls: 'num', v: (f) => f.idx + 1 },
    { k: 'name', t: '파일 이름', v: (f) => f.name || h('span', { class: 'mu' }, `file${f.idx + 1}`) },
    { k: 'size', t: '크기', cls: 'num', v: (f) => bytes(f.size) },
    { k: 'act', t: '', sort: false, cls: 'act', v: (f) => (kept(f)
      ? h('a', { class: 'btn sm', href: href(f), download: f.name || '' }, '다운로드')
      : h('span', { class: 'mu' }, '보관 안 됨')) },
  ], files, { key: 'idx', empty: '첨부파일 없음' });
}
const recipientNames = (rs) => (rs || []).map((x) => x.username || x.name || short(x.account)).join(', ');

// A message still waiting on the relay. Opening a plaintext one is audited server-side.
async function openMessage(mid, reload) {
  let m;
  try { m = await api('GET', `/api/messages/${mid}`); } catch (e) { return fail(e); }
  const plain = m.enc === 'none';
  const acked = m.deliveries.filter((d) => d.acked).length;
  const dr = drawer(`보관 쪽지 · ${m.from_name}`, [
    h('div', { class: 'kv' },
      h('div', {}, '보낸 사람'), h('div', {}, h('a', { href: `#/accounts?state=all&id=${m.from}` }, m.from_name || short(m.from)), ' ', h('span', { class: 'mono mu' }, m.from_device)),
      h('div', {}, '보관 시작'), h('div', {}, tsFull(m.created)),
      h('div', {}, '쪽지 만료'), h('div', {}, until(m.text_expires)),
      h('div', {}, '첨부 만료'), h('div', {}, m.files.length ? (m.files_gone ? '삭제됨' : until(m.files_expires)) : '—'),
      h('div', {}, '본문'), h('div', {}, plain ? chip('ne', '평문') : chip('ok', '종단간 암호화')),
      h('div', {}, '쪽지 ID'), h('div', { class: 'mono' }, m.mid)),
    plain
      ? [h('div', { class: 'panel' }, h('div', { class: 'hd' }, '내용'), h('div', { class: 'bd' }, msgBody(m.content.text))), groupBox(m.content.group)]
      : h('div', { class: 'warnbox' }, '종단간 암호화된 쪽지입니다. 서버는 본문, 파일 이름, 첨부파일 내용을 볼 수 없습니다.'),
    h('div', { class: 'panel' }, h('div', { class: 'hd' }, `첨부파일 ${m.files.length}개`), h('div', { class: 'bd flush' },
      fileTable('msg-files', m.files, (f) => `/api/messages/${m.mid}/files/${f.idx}`, (f) => f.available))),
    h('div', { class: 'panel' }, h('div', { class: 'hd' }, `전달 상태 · ${acked}/${m.deliveries.length} 기기 수신`), h('div', { class: 'bd flush' },
      grid('msg-deliv', [
        { k: 'account_name', t: '받는 사람', v: (d) => h('a', { href: `#/accounts?state=all&id=${d.account}` }, d.account_name || short(d.account)) },
        { k: 'device_name', t: '기기', v: (d) => [d.device_name || '—', ' ', h('span', { class: 'mono mu' }, d.device)] },
        { k: 'acked', t: '수신', v: (d) => (d.acked ? h('span', { title: tsFull(d.acked) }, chip('ok', '받음'), ' ', ts(d.acked)) : chip('wa', '대기')) },
      ], m.deliveries, { key: 'device' }))),
  ], [
    h('span', { class: 'mu' }, plain ? '이 열람은 감사 로그에 기록되었습니다' : ''), h('span', { class: 'sp' }),
    h('button', { class: 'btn', onclick: () => dr.close() }, '닫기'),
  ]);
}

// ---------------------------------------------------------------- message audit (archive)
async function pArchive(c, ph) {
  const { settings } = await api('GET', '/api/settings');
  const day = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const from = h('input', { type: 'date', value: day(new Date(Date.now() - 7 * 86400000)) });
  const to = h('input', { type: 'date', value: day(new Date()) });
  const q = h('input', { type: 'search', placeholder: '본문, 보낸 사람, 받는 사람, 파일 이름' });
  const lim = h('select', {}, [200, 1000, 5000, 20000].map((n) => h('option', { value: n, selected: n === 1000 }, `최대 ${num(n)}건`)));
  const go = h('button', { class: 'btn pri', onclick: load }, '조회');
  const host = h('div', { class: 'empty' }, '조건을 정하고 조회하세요. 조회할 때마다 감사 로그에 기록됩니다.');
  const foot = h('div', { class: 'foot' });
  let rows = [];
  q.addEventListener('keydown', (e) => { if (e.key === 'Enter') load(); });
  ph.append(h('span', { class: 'sub' }, '종단간 암호화가 아닌 쪽지의 사본 (전달 후에도 보관)'), h('span', { class: 'sp' }),
    h('button', {
      class: 'btn',
      onclick: () => csv(`relay-messages-${day(new Date())}.csv`, ['time', 'from', 'from_account', 'to', 'text', 'files', 'bytes', 'id'],
        rows.map((r) => [tsFull(r.created), r.from_name, r.from, recipientNames(r.recipients), r.preview + (r.length > r.preview.length ? '…' : ''), r.files.map((f) => f.name).join(' | '), r.bytes, r.mid])),
    }, 'CSV 내보내기'));
  async function load() {
    go.disabled = true;
    try {
      const f = from.value ? new Date(`${from.value}T00:00:00`).getTime() : '';
      const t = to.value ? new Date(`${to.value}T00:00:00`).getTime() + 86400000 : '';
      const qs = new URLSearchParams({ q: q.value.trim(), limit: lim.value, ...(f ? { from: f } : {}), ...(t ? { to: t } : {}) });
      const r = await api('GET', `/api/archive?${qs}`);
      rows = r.archive;
      host.replaceChildren(grid('archive', [
        { k: 'created', t: '보낸 시각', v: (x) => h('span', { title: tsFull(x.created) }, ts(x.created)) },
        { k: 'from_name', t: '보낸 사람', v: (x) => x.from_name || short(x.from) },
        { k: 'to', t: '받는 사람', cls: 'trunc', s: (x) => recipientNames(x.recipients), v: (x) => h('span', { title: recipientNames(x.recipients) }, recipientNames(x.recipients)) },
        { k: 'preview', t: '내용', cls: 'trunc wide', v: (x) => h('span', { title: x.preview }, x.preview.replace(/\s+/g, ' ') || h('span', { class: 'mu' }, '(본문 없음)')) },
        { k: 'group', t: '그룹', s: (x) => x.group?.name, v: (x) => x.group?.name || '' },
        { k: 'files', t: '첨부', cls: 'num', s: (x) => x.files.length, v: (x) => (x.files.length ? `${x.files.length}개${x.files_kept ? '' : ' (미보관)'}` : '') },
        { k: 'bytes', t: '크기', cls: 'num', v: (x) => (x.bytes ? bytes(x.bytes) : '') },
      ], rows, { key: 'mid', onRow: (x) => openArchived(x.mid, load), empty: '조건에 맞는 쪽지가 없습니다' }));
      foot.textContent = `${num(rows.length)}건 표시 · 조건에 맞는 ${num(r.total)}건 · 전체 보관 ${num(r.all)}건`;
    } catch (e) { fail(e); } finally { go.disabled = false; }
  }
  c.append(...[
    !settings.archive && h('div', { class: 'warnbox' }, '감사 보관이 꺼져 있어 새 쪽지는 보관되지 않습니다. ', h('a', { href: '#/policy' }, '정책 > 쪽지 감사'), '에서 켤 수 있습니다. 지금 서버에 맡겨진 쪽지는 "보관함"에서 열람할 수 있습니다.'),
    settings.archive && settings.policy.encryption === 'e2ee' && h('div', { class: 'warnbox' }, '현재 암호화 단계가 종단간 암호화라서 새 쪽지는 서버가 내용을 볼 수 없어 보관되지 않습니다.'),
    h('div', { class: 'tb' }, h('span', { class: 'mu' }, '기간'), from, h('span', { class: 'mu' }, '~'), to, q, lim, go),
    host, foot].filter(Boolean));
}

async function openArchived(mid, reload) {
  let m;
  try { m = await api('GET', `/api/archive/${mid}`); } catch (e) { return fail(e); }
  const dr = drawer(`쪽지 감사 · ${m.from_name}`, [
    h('div', { class: 'kv' },
      h('div', {}, '보낸 사람'), h('div', {}, h('a', { href: `#/accounts?state=all&id=${m.from}` }, m.from_name || short(m.from)), ' ', h('span', { class: 'mono mu' }, m.from_device)),
      h('div', {}, '받는 사람'), h('div', {}, (m.recipients || []).map((x, i) => [i ? ', ' : '', h('a', { href: `#/accounts?state=all&id=${x.account}` }, x.username || x.name || short(x.account)), h('span', { class: 'mu' }, ` (${x.devices}대)`)])),
      h('div', {}, '보낸 시각'), h('div', {}, tsFull(m.created)),
      h('div', {}, '쪽지 ID'), h('div', { class: 'mono' }, m.mid)),
    h('div', { class: 'panel' }, h('div', { class: 'hd' }, '내용'), h('div', { class: 'bd' }, msgBody(m.text))),
    groupBox(m.group),
    h('div', { class: 'panel' }, h('div', { class: 'hd' }, `첨부파일 ${m.files.length}개`, m.files.length && !m.files_kept ? h('span', { class: 'mu' }, ' · 첨부파일은 보관하지 않도록 설정되어 있었습니다') : ''), h('div', { class: 'bd flush' },
      fileTable('arc-files', m.files, (f) => `/api/archive/${m.mid}/files/${f.idx}`, () => m.files_kept))),
  ], [
    h('button', { class: 'btn dan', onclick: del }, '감사 사본 삭제'), h('span', { class: 'mu' }, '이 열람은 감사 로그에 기록되었습니다'), h('span', { class: 'sp' }),
    h('button', { class: 'btn', onclick: () => dr.close() }, '닫기'),
  ]);
  async function del() {
    if (!(await confirmBox('감사 사본 삭제', ['이 쪽지의 감사 사본(본문과 보관된 첨부파일)을 삭제할까요? 삭제 기록은 감사 로그에 남습니다.'], '삭제', true))) return;
    try { await api('DELETE', `/api/archive/${m.mid}`); toast('삭제했습니다'); dr.close(); reload(); } catch (e) { fail(e); }
  }
}

// ---------------------------------------------------------------- audit
const ACTIONS = {
  admin_setup: '최초 관리자 생성', admin_login: '관리자 로그인', admin_login_failed: '관리자 로그인 실패', admin_create: '관리자 추가',
  admin_delete: '관리자 삭제', admin_password: '관리자 비밀번호 변경', settings: '정책 변경', account_create: '계정 생성',
  account_update: '계정 수정', account_delete: '계정 삭제', account_approve: '계정 승인', account_enable: '계정 정지 해제',
  account_disable: '계정 정지', register: '가입', device_add: '기기 추가', device_move: '기기 계정 이동', device_disable: '기기 정지',
  device_enable: '기기 정지 해제', device_delete: '기기 등록 해제', device_kick: '연결 끊기', message_purge: '보관 쪽지 삭제', sweep: '만료 처리 실행', login_locked: '로그인 잠금',
  message_view: '보관 쪽지 열람', message_file: '보관 첨부 다운로드', archive_list: '쪽지 감사 조회', archive_view: '쪽지 감사 열람',
  archive_file: '감사 첨부 다운로드', archive_delete: '감사 사본 삭제',
};
async function pAudit(c, ph) {
  let limit = 1000;
  let rows = [];
  const q = h('input', { type: 'search', placeholder: '주체, 내용', oninput: () => draw() });
  const act = h('select', { onchange: () => draw() });
  const lim = h('select', { onchange: () => { limit = Number(lim.value); load(); } }, [500, 1000, 5000, 20000].map((n) => h('option', { value: n, selected: n === limit }, `최근 ${num(n)}건`)));
  const host = h('div');
  const foot = h('div', { class: 'foot' });
  let shown = [];
  ph.append(h('span', { class: 'sp' }), h('button', {
    class: 'btn',
    onclick: () => csv(`relay-audit-${ts(Date.now()).replace(/[: ]/g, '')}.csv`, ['time', 'actor', 'action', 'detail'], shown.map((r) => [tsFull(r.ts), r.actor, r.action, r.detail])),
  }, 'CSV 내보내기'));
  async function load() {
    rows = (await api('GET', `/api/audit?limit=${limit}`)).audit;
    const cur = act.value;
    act.replaceChildren(h('option', { value: '' }, '모든 작업'), [...new Set(rows.map((r) => r.action))].sort().map((a) => h('option', { value: a, selected: a === cur }, ACTIONS[a] || a)));
    draw();
  }
  function draw() {
    const s = q.value.trim().toLowerCase();
    shown = rows.filter((r) => (!act.value || r.action === act.value) && (!s || `${r.actor} ${r.detail}`.toLowerCase().includes(s)));
    host.replaceChildren(grid('audit', [
      { k: 'ts', t: '시각', v: (r) => h('span', { class: 'mono' }, tsFull(r.ts)) },
      { k: 'actor', t: '주체', cls: 'mono' },
      { k: 'action', t: '작업', v: (r) => h('span', { title: r.action }, ACTIONS[r.action] || r.action) },
      { k: 'detail', t: '내용', cls: 'wrap' },
    ], shown, { key: 'ts', empty: '기록이 없습니다' }));
    foot.textContent = `${shown.length}건 표시 · 불러온 ${rows.length}건`;
  }
  c.append(h('div', { class: 'tb' }, act, q, h('span', { class: 'sp' }), lim), host, foot);
  await load();
}

// ---------------------------------------------------------------- policy
const POLICY = [
  ['서버', [
    { p: 'server_name', t: '서버 이름', type: 'text', help: '클라이언트와 이 관리 화면에 표시됩니다' },
    { p: 'policy.contacts', t: '연락처 공개 범위', type: 'select', opts: [['all', '전체 공개 — 모든 사용자가 서로 보임'], ['lookup', '검색 — 주고받았거나 직접 찾은 사용자만']], help: '사용자가 많은 공개 서버라면 "검색"을 권장합니다' },
    { p: 'show_host', t: '컴퓨터 이름 공개', type: 'bool', label: '연락처에 기기의 컴퓨터(호스트) 이름 포함' },
  ]],
  ['가입 및 로그온', [
    { p: 'policy.auth', t: '로그온 방식', type: 'select', opts: [['password', '사용자 이름 + 비밀번호'], ['none', '없음 — 기기 ID와 기기 비밀값만']], warn: '바꾸면 접속 중인 모든 기기의 연결이 끊기고, 다시 로그온해야 합니다', help: '"없음"이면 기기 하나가 계정 하나가 됩니다' },
    { p: 'policy.registration', t: '외부인 가입', type: 'select', opts: [['closed', '닫힘 — 관리자가 만든 계정만 (기본)'], ['approval', '승인제 — 신청을 받고 관리자가 승인'], ['open', '열림 — 누구나 바로 가입 (공개 서버용)']] },
    { p: 'policy.max_devices', t: '계정당 기기 수', type: 'num', min: 1, max: 9999, unit: '대', help: '계정마다 따로 정할 수 있습니다. 최대 9999대' },
    { p: 'min_password_len', t: '최소 비밀번호 길이', type: 'num', min: 4, max: 128, unit: '자', help: '관리자 계정은 8자 이상' },
    { p: 'login_max_failures', t: '로그인 실패 허용', type: 'num', min: 1, max: 1000, unit: '회', help: '사용자 이름별·기기별로 셉니다' },
    { p: 'login_lockout_minutes', t: '잠금 시간', type: 'num', min: 1, max: 10080, unit: '분' },
  ]],
  ['경로', [
    { p: 'policy.force_relay', t: '릴레이 경유 강제', type: 'bool', label: '이 서버 연락처에 있는 사람에게는 LAN에서 보여도 릴레이로 보냄' },
    { p: 'policy.block_direct_outsiders', t: '외부 LAN 직접 전송', type: 'bool', label: '이 서버를 쓰지 않는 LAN 사용자에게 직접 보내는 것도 금지', dep: 'policy.force_relay', help: '경유 강제일 때만 적용됩니다. 서버는 LAN 전송을 볼 수 없으므로 클라이언트가 지킵니다' },
  ]],
  ['암호화', [
    { p: 'policy.encryption', t: '암호화 단계', type: 'select', opts: [['plain', '평문 — 암호화 없음'], ['tls', '서버 통신 암호화 — TLS 필수, 서버에는 평문 저장'], ['e2ee', '종단간 암호화 — TLS 필수, 서버는 내용을 볼 수 없음']], warn: '평문이 아닌 단계로 바꾸면 평문으로 접속한 기기의 연결이 끊깁니다', help: 'LAN 직접 전송에는 적용되지 않습니다' },
  ]],
  ['전송 한도 (릴레이를 거칠 때만)', [
    { p: 'policy.relay_max_files', t: '첨부파일 수', type: 'num', min: 0, max: 100000, unit: '개', help: '0이면 첨부파일을 중계하지 않습니다' },
    { p: 'policy.relay_max_file_size', t: '파일당 최대 크기', type: 'mb', unit: 'MB', help: '0이면 첨부파일을 중계하지 않습니다. E2EE에서는 암호문 크기 기준' },
    { p: 'max_body_kb', t: '본문 최대 크기', type: 'num', min: 1, max: 7168, unit: 'KB', help: '본문, 파일 이름, 그룹 정보(또는 암호문)를 합친 크기' },
  ]],
  ['보관', [
    { p: 'policy.retention_hours.text', t: '쪽지 보관 시간', type: 'num', min: 1, max: 87600, unit: '시간', help: '지나면 삭제하고, 받지 못한 기기에는 보낸 사람별 만료 개수만 알립니다. 계정의 모든 기기가 받아야 전달 완료로 봅니다' },
    { p: 'policy.retention_hours.files', t: '첨부파일 보관 시간', type: 'num', min: 1, max: 87600, unit: '시간', help: '쪽지 보관 시간보다 길게 정해도 쪽지 보관 시간으로 맞춰집니다' },
    { p: 'storage_quota_mb', t: '전체 저장 한도', type: 'num', min: 0, unit: 'MB', help: '첨부파일 기준. 0이면 무제한' },
    { p: 'account_quota_mb', t: '계정별 저장 한도', type: 'num', min: 0, unit: 'MB', help: '보낸 계정 기준. 0이면 무제한. 계정마다 따로 정할 수 있습니다' },
  ]],
  ['쪽지 감사', [
    { p: 'archive', t: '감사 보관', type: 'bool', label: '종단간 암호화가 아닌 쪽지의 사본을 전달 후에도 보관', warn: '사용자에게는 알리지 않습니다. 조직의 정보 보호 방침에 따라 쓰세요', help: '켠 뒤부터 릴레이를 거치는 쪽지가 보관됩니다. LAN 직접 전송은 서버를 거치지 않아 보관되지 않습니다' },
    { p: 'archive_files', t: '첨부파일 보관', type: 'bool', label: '감사 사본에 첨부파일도 함께 보관', dep: 'archive', help: '디스크를 많이 쓸 수 있습니다. 끄면 파일 이름과 크기만 남습니다' },
    { p: 'archive_days', t: '감사 보관 기간', type: 'num', min: 1, max: 3650, unit: '일', help: '지나면 사본과 첨부파일을 삭제합니다' },
  ]],
  ['연결 및 관리', [
    { p: 'idle_timeout_secs', t: '무응답 연결 끊기', type: 'num', min: 45, max: 3600, unit: '초', help: '클라이언트는 30초마다 ping을 보냅니다' },
    { p: 'admin_session_hours', t: '관리자 세션 유지', type: 'num', min: 1, max: 720, unit: '시간', help: '마지막 사용 시각부터' },
    { p: 'audit_days', t: '감사 로그 보관', type: 'num', min: 1, max: 3650, unit: '일' },
  ]],
];
const getP = (o, p) => p.split('.').reduce((a, k) => (a == null ? a : a[k]), o);
const setP = (o, p, v) => { const ks = p.split('.'); const last = ks.pop(); ks.reduce((a, k) => a[k], o)[last] = v; };
const MB = 1024 * 1024;

async function pPolicy(c, ph) {
  let orig = (await api('GET', '/api/settings')).settings;
  let w = structuredClone(orig);
  const rows = [];
  const dirtyInfo = h('span', { class: 'mu' });
  const saveBtn = h('button', { class: 'btn pri', disabled: true, onclick: save }, '저장');
  const revertBtn = h('button', { class: 'btn', disabled: true, onclick: () => { w = structuredClone(orig); build(); } }, '되돌리기');
  ph.append(h('span', { class: 'sub' }, '저장하면 접속 중인 클라이언트에 바로 알립니다'));
  const form = h('div', { class: 'form' });
  const note = h('div');

  const label = (it, v) => {
    if (it.type === 'bool') return v ? '켬' : '끔';
    if (it.type === 'select') return (it.opts.find((o) => o[0] === v) || [, v])[1].split(' — ')[0];
    if (it.type === 'mb') return `${+(v / MB).toFixed(2)} MB`;
    return `${v}${it.unit ? ' ' + it.unit : ''}`;
  };
  function changes() {
    return POLICY.flatMap(([, items]) => items).filter((it) => JSON.stringify(getP(w, it.p)) !== JSON.stringify(getP(orig, it.p)));
  }
  function refresh() {
    const ch = changes();
    rows.forEach(([it, row, ctl]) => {
      row.classList.toggle('dirty', ch.includes(it));
      if (it.dep) ctl.disabled = !getP(w, it.dep);
    });
    saveBtn.disabled = revertBtn.disabled = !ch.length;
    dirtyInfo.textContent = ch.length ? `변경 ${ch.length}건` : '변경 사항 없음';
    const warns = [];
    if (w.policy.retention_hours.files > w.policy.retention_hours.text) warns.push('첨부파일 보관 시간이 쪽지 보관 시간보다 깁니다. 저장하면 쪽지 보관 시간으로 맞춰집니다.');
    if (w.policy.registration === 'open') warns.push('외부인 가입이 "열림"입니다. 이 서버에 접속할 수 있는 누구나 계정을 만들 수 있습니다.');
    if (w.policy.encryption === 'plain') warns.push('암호화가 "평문"입니다. 신뢰할 수 있는 내부망에서만 쓰세요.');
    if (w.archive && w.policy.encryption === 'e2ee') warns.push('감사 보관이 켜져 있지만 암호화 단계가 종단간 암호화라서 쪽지 내용은 보관되지 않습니다.');
    if (w.policy.auth === 'none' && w.policy.registration === 'open') warns.push('로그온 방식 "없음" + 가입 "열림": 서버에 닿는 모든 기기가 바로 사용자가 됩니다.');
    note.replaceChildren(...warns.map((t) => h('div', { class: 'warnbox' }, t)));
  }
  function control(it) {
    const v = getP(w, it.p);
    const set = (x) => { setP(w, it.p, x); refresh(); };
    if (it.type === 'bool') {
      const cb = h('input', { type: 'checkbox', checked: !!v, onchange: () => set(cb.checked) });
      return [cb, h('label', { class: 'line' }, cb, it.label || '')];
    }
    if (it.type === 'select') {
      const s = h('select', { onchange: () => set(s.value) }, it.opts.map(([k, t]) => h('option', { value: k, selected: k === v }, t)));
      return [s, s];
    }
    if (it.type === 'text') {
      const i = h('input', { type: 'text', value: v, oninput: () => set(i.value) });
      return [i, i];
    }
    if (it.type === 'mb') {
      const i = h('input', { type: 'number', min: 0, step: 'any', value: +(v / MB).toFixed(2), oninput: () => set(Math.max(0, Math.round((Number(i.value) || 0) * MB))) });
      return [i, h('span', { class: 'line' }, i, h('span', { class: 'unit' }, it.unit))];
    }
    const i = h('input', { type: 'number', min: it.min, max: it.max, value: v, oninput: () => set(Math.max(it.min ?? 0, Math.min(it.max ?? Number.MAX_SAFE_INTEGER, Math.floor(Number(i.value) || 0)))) });
    return [i, h('span', { class: 'line' }, i, h('span', { class: 'unit' }, it.unit || ''), it.max && h('span', { class: 'mu' }, `(${num(it.min)}~${num(it.max)})`))];
  }
  function build() {
    rows.length = 0;
    form.replaceChildren(...POLICY.map(([title, items]) => h('div', { class: 'fs' }, h('div', { class: 'hd' }, title), items.map((it) => {
      const [ctl, node] = control(it);
      const row = h('div', { class: 'row' }, h('label', {}, it.t), h('div', { class: 'ctl' }, node,
        it.help && h('div', { class: 'help' }, it.help),
        it.warn && h('div', { class: 'help err' }, '⚠ ' + it.warn)));
      rows.push([it, row, ctl]);
      return row;
    }))));
    refresh();
  }
  async function save() {
    const ch = changes();
    const lines = ch.map((it) => h('li', {}, h('b', {}, it.t), `: ${label(it, getP(orig, it.p))} → ${label(it, getP(w, it.p))}`));
    const warn = ch.filter((it) => it.warn).map((it) => h('div', { class: 'warnbox' }, it.warn));
    if (!(await confirmBox('정책 저장', [h('ul', {}, lines), ...warn], '저장'))) return;
    try {
      const r = await api('PUT', '/api/settings', w);
      orig = r.settings;
      w = structuredClone(orig);
      build();
      toast(`정책을 저장했습니다 (${r.changes.length}건)`);
    } catch (e) { fail(e); }
  }
  build();
  c.append(note, form);
  c.parentNode.append(h('div', { class: 'savebar' }, dirtyInfo, h('span', { class: 'sp' }), revertBtn, saveBtn));
}

// ---------------------------------------------------------------- admins
async function pAdmins(c) {
  const host = h('div');
  async function load() {
    const { admins } = await api('GET', '/api/admins');
    host.replaceChildren(grid('admins', [
      { k: 'username', t: '이름', v: (r) => [r.username, r.me && h('span', {}, ' ', chip('ac', '나'))] },
      { k: 'created', t: '생성', v: (r) => ts(r.created) },
      { k: 'last_login', t: '마지막 로그인', v: (r) => ago(r.last_login) },
      { k: 'act', t: '', sort: false, cls: 'act', v: (r) => !r.me && h('button', { class: 'btn sm dan', onclick: () => del(r) }, '삭제') },
    ], admins));
  }
  async function del(a) {
    if (!(await confirmBox('관리자 삭제', [`${a.username} 관리자를 삭제할까요? 로그인 중이면 바로 로그아웃됩니다.`], '삭제', true))) return;
    try { await api('DELETE', `/api/admins/${a.id}`); toast('삭제했습니다'); load(); } catch (e) { fail(e); }
  }
  async function add() {
    const u = h('input', { type: 'text', autocomplete: 'off' });
    const p = h('input', { type: 'password', autocomplete: 'new-password' });
    if (await modal('관리자 추가', [field('이름', u), field('비밀번호 (8자 이상)', p)], { ok: '추가', onOk: () => api('POST', '/api/admins', { username: u.value.trim(), password: p.value }) })) {
      toast('추가했습니다');
      load();
    }
  }
  const old = h('input', { type: 'password', autocomplete: 'current-password' });
  const n1 = h('input', { type: 'password', autocomplete: 'new-password' });
  const n2 = h('input', { type: 'password', autocomplete: 'new-password' });
  async function change() {
    if (n1.value !== n2.value) return fail('새 비밀번호가 서로 다릅니다');
    try { await api('POST', '/api/password', { old: old.value, new: n1.value }); toast('비밀번호를 바꿨습니다'); old.value = n1.value = n2.value = ''; } catch (e) { fail(e); }
  }
  const row = (label, ctl) => h('div', { class: 'row' }, h('label', {}, label), h('div', { class: 'ctl' }, ctl));
  c.append(
    h('div', { class: 'cols' },
      h('div', { class: 'panel' }, h('div', { class: 'hd' }, '관리자 계정', h('span', { class: 'sp' }), h('button', { class: 'btn sm pri', onclick: add }, '추가')), h('div', { class: 'bd flush' }, host)),
      h('div', { class: 'fs' }, h('div', { class: 'hd' }, `내 비밀번호 변경`, h('span', { class: 'mu' }, ME)),
        row('현재 비밀번호', old), row('새 비밀번호', n1), row('새 비밀번호 확인', n2),
        h('div', { class: 'row' }, h('span'), h('div', { class: 'ctl' }, h('div', { class: 'line' }, h('button', { class: 'btn pri', onclick: change }, '변경')))))),
    h('div', { class: 'mu' }, '관리 화면은 기본적으로 서버 자신(127.0.0.1)에만 열려 있습니다. 원격에서 쓰려면 HTTPS 리버스 프록시나 SSH 터널을 거치세요.'));
  await load();
}

authScreen();
