'use strict';
const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;
const win = T.window.getCurrentWindow();
let V = {}; // set by boot()
const $app = document.getElementById('app');

// Tiny DOM helper. Strings become text nodes, so message content is never parsed as HTML.
function h(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (k in e && (typeof v !== 'string' || k === 'value')) e[k] = v; // textarea ignores the value attribute
    else if (v != null && v !== false) e.setAttribute(k, v);
  }
  for (const c of kids.flat()) if (c != null && c !== false) e.append(c instanceof Node ? c : String(c));
  return e;
}
const render = (...els) => $app.replaceChildren(...els.flat().filter(Boolean));
const size = (n) => {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i ? `${n.toFixed(1)} ${u[i]}` : `${n} B`;
};
const time = (ts) => {
  const d = new Date(ts), now = new Date();
  const hm = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  return d.toDateString() === now.toDateString() ? hm : `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
};
const base = (p) => p.split(/[\\/]/).pop();
const close = () => win.close();
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input,textarea,.body')) e.preventDefault(); });

// Splits text into text nodes and http(s) links. The link label is the normalized
// URL itself (IDN hosts show as xn--), so what you see is what opens; the backend
// re-validates before handing it to the browser.
function linkify(text) {
  const out = [];
  let last = 0;
  for (const m of text.matchAll(/https?:\/\/[^\s<>"'`]+/gi)) {
    const raw = m[0].replace(/[.,;:!?'")\]}>]+$/, '');
    let url;
    try { url = new URL(raw); } catch { continue; }
    if (url.username || url.password || !url.hostname) continue;
    out.push(text.slice(last, m.index));
    const href = url.href;
    const err = (e) => { a.title = String(e); a.classList.add('err'); };
    const a = h('a', { title: href, onclick: () => invoke('open_url', { url: href }).catch(err) }, href);
    out.push(a);
    last = m.index + raw.length;
  }
  out.push(text.slice(last));
  return out;
}

// ---------------------------------------------------------------- popup
async function popup() {
  const m = await invoke('get_message', { id: V.id });
  if (!m) return close();
  const reply = () => { invoke('open_compose', { to: [m.from_id], files: [], reply: m.id }); close(); };
  render(
    h('div', { class: 'head' }, h('b', {}, m.from_name), h('span', { class: 'muted' }, `${m.from_ip} · ${time(m.ts)}`)),
    h('div', { class: 'body grow' }, linkify(m.text)),
    m.files.length && h('div', { class: 'files' }, m.files.map((f) =>
      h('div', { class: 'file' },
        h('a', { title: f.path, onclick: () => invoke('open_path', { path: f.path, reveal: false }) }, f.name),
        h('span', { class: 'muted' }, size(f.size)),
        h('a', { class: 'muted', onclick: () => invoke('open_path', { path: f.path, reveal: true }) }, '폴더')))),
    m.note && h('div', { class: 'err' }, m.note),
    h('div', { class: 'bar' },
      h('button', { class: 'primary', onclick: reply }, '답장'),
      h('button', { onclick: close }, '닫기')),
  );
  document.addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.ctrlKey) reply(); });
}

// ---------------------------------------------------------------- compose
async function compose() {
  const peers = await invoke('peers');
  let to = V.to.slice();
  let files = V.files.slice();
  const nameOf = (id) => (peers.find((p) => p.id === id) || { name: id }).name;
  const { signature } = await invoke('get_settings');
  const orig = V.reply ? await invoke('get_message', { id: V.reply }) : null;
  // Cursor goes on top; signature and the quoted original sit below three blank lines.
  const below = [signature.trimEnd(), orig && quote(orig)].filter(Boolean).join('\n\n');
  const initial = below ? `\n\n\n${below}` : '';

  const toLine = h('div', { class: 'muted' });
  const ta = h('textarea', { class: 'grow', placeholder: '내용 (Ctrl+Enter 전송)', value: initial });
  const list = h('div', { class: 'files' });
  const prog = h('progress', { max: 1, value: 0, hidden: true });
  const status = h('span', { class: 'status muted' });
  const sendBtn = h('button', { class: 'primary', onclick: send }, '전송');

  const drawTo = () => { toLine.textContent = `받는 사람: ${to.map(nameOf).join(', ')}`; };
  const drawFiles = () => list.replaceChildren(...files.map((f, i) =>
    h('div', { class: 'file' },
      h('span', { title: f }, base(f)),
      h('a', { class: 'muted', onclick: () => { files.splice(i, 1); drawFiles(); } }, '✕'))));
  const addFiles = (ps) => { files = [...new Set([...files, ...ps])]; drawFiles(); };

  async function send() {
    if (sendBtn.disabled || ((!ta.value.trim() || ta.value === initial) && !files.length)) return;
    sendBtn.disabled = true;
    status.className = 'status muted';
    status.textContent = '보내는 중…';
    const job = Math.random().toString(36).slice(2);
    const un = await listen('send-progress', ({ payload: p }) => {
      if (p.job !== job) return;
      prog.hidden = false;
      prog.value = p.total ? p.done / p.total : 0;
      status.textContent = `${p.to}: ${size(p.done)} / ${size(p.total)}`;
    });
    const res = await invoke('send_message', { job, to, text: ta.value, files });
    un();
    const failed = res.filter((r) => !r.ok);
    if (!failed.length) return close();
    to = failed.map((r) => r.to); // retry only the failures
    drawTo();
    prog.hidden = true;
    status.className = 'status err';
    status.textContent = failed.map((r) => `${r.name}: ${r.error}`).join('\n');
    sendBtn.disabled = false;
  }

  ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.ctrlKey) send(); });
  T.webview.getCurrentWebview().onDragDropEvent(({ payload: p }) => {
    $app.classList.toggle('drop', p.type === 'enter' || p.type === 'over');
    if (p.type === 'drop') addFiles(p.paths);
  });
  drawTo();
  drawFiles();
  render(toLine, ta, list, prog,
    h('div', { class: 'bar' }, status,
      h('button', { onclick: async () => addFiles(await invoke('pick_files')) }, '파일…'),
      sendBtn));
  ta.focus();
  ta.setSelectionRange(0, 0);
  ta.scrollTop = 0;
}

function quote(m) {
  const head = ['----- 원본 쪽지 -----', `보낸 사람: ${m.from_name} (${new Date(m.ts).toLocaleString()})`];
  return [...head, ...m.text.split('\n').map((l) => `> ${l}`)].join('\n');
}

// ---------------------------------------------------------------- recipient picker
async function pick() {
  let peers = await invoke('peers');
  const sel = new Set();
  const q = h('input', { placeholder: '이름 / 호스트 / IP 검색', oninput: draw });
  const list = h('div', { class: 'grow' });
  const go = h('button', { class: 'primary', disabled: true, onclick: () => open([...sel]) }, '쓰기');
  function open(ids) {
    if (!ids.length) return;
    invoke('open_compose', { to: ids, files: V.files });
    close();
  }
  function draw() {
    const s = q.value.trim().toLowerCase();
    const shown = peers.filter((p) => !s || `${p.name} ${p.host} ${p.ip}`.toLowerCase().includes(s));
    list.replaceChildren(...(shown.length ? shown.map((p) => {
      const cb = h('input', { type: 'checkbox', checked: sel.has(p.id), onchange: () => {
        cb.checked ? sel.add(p.id) : sel.delete(p.id);
        go.disabled = !sel.size;
        go.textContent = sel.size ? `${sel.size}명에게 쓰기` : '쓰기';
      } });
      return h('div', { class: 'row' + (p.online ? '' : ' off') }, cb,
        h('span', { class: 'name', title: '클릭하면 바로 쓰기', onclick: () => open([p.id]) }, p.name),
        h('span', { class: 'muted' }, p.online ? (p.host || p.ip) : '오프라인'));
    }) : [h('div', { class: 'empty muted' }, '사용자가 없습니다')]));
  }
  listen('peers', async () => { peers = await invoke('peers'); draw(); });
  q.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    const first = list.querySelector('.name');
    if (sel.size) open([...sel]); else if (first) first.click();
  });
  draw();
  render(
    V.files.length && h('div', { class: 'muted' }, `파일 ${V.files.length}개: ${V.files.map(base).join(', ')}`),
    q, list, h('div', { class: 'bar' }, go));
  q.focus();
}

// ---------------------------------------------------------------- inbox / sent
async function inbox() {
  let tab = 'in';
  const list = h('div', { class: 'grow' });
  const tIn = h('button', { onclick: () => show('in') }, '받은 쪽지');
  const tOut = h('button', { onclick: () => show('out') }, '보낸 쪽지');
  const readAll = h('button', { onclick: () => invoke('mark_all_read') }, '모두 읽음');
  const stop = (fn) => (e) => { e.stopPropagation(); fn(); };
  const preview = (text, files) => (files.length ? `[${files.length}] ` : '') + (text.split('\n')[0] || '');

  async function drawIn() {
    const ms = await invoke('inbox');
    const unread = ms.filter((m) => !m.read).length;
    tIn.textContent = unread ? `받은 쪽지 (${unread})` : '받은 쪽지';
    if (tab !== 'in') return;
    list.replaceChildren(...(ms.length ? ms.map((m) =>
      h('div', { class: 'row msg' + (m.read ? '' : ' unread'), onclick: () => invoke('open_message', { id: m.id }) },
        h('span', { class: 'name' }, m.from_name),
        h('span', { class: 'preview' }, preview(m.text, m.files)),
        h('span', { class: 'muted' }, time(m.ts)),
        h('a', { onclick: stop(() => invoke('open_compose', { to: [m.from_id], files: [], reply: m.id })) }, '답장'),
        h('a', { class: 'muted', title: '삭제', onclick: stop(() => invoke('delete_message', { id: m.id })) }, '✕')))
      : [h('div', { class: 'empty muted' }, '받은 쪽지가 없습니다')]));
  }
  async function drawOut() {
    if (tab !== 'out') return;
    const ss = await invoke('sent_list');
    list.replaceChildren(...(ss.length ? ss.map((s) => {
      const failed = s.to.some((r) => !r.ok);
      return h('div', { class: 'row msg', onclick: () => invoke('open_sent', { id: s.id }) },
        h('span', { class: 'name', title: s.to.map((r) => r.name).join(', ') }, '→ ' + s.to.map((r) => r.name).join(', ')),
        h('span', { class: 'preview' }, preview(s.text, s.files)),
        failed && h('span', { class: 'err' }, '실패'),
        h('span', { class: 'muted' }, time(s.ts)),
        h('a', { class: 'muted', title: '삭제', onclick: stop(() => invoke('delete_sent', { id: s.id })) }, '✕'));
    }) : [h('div', { class: 'empty muted' }, '보낸 쪽지가 없습니다')]));
  }
  function show(t) {
    tab = t;
    tIn.classList.toggle('on', t === 'in');
    tOut.classList.toggle('on', t === 'out');
    readAll.hidden = t !== 'in';
    return t === 'in' ? drawIn() : drawOut();
  }
  listen('inbox', drawIn);
  listen('sent', drawOut);
  await show('in');
  render(h('div', { class: 'tabs' }, tIn, tOut), list, h('div', { class: 'bar' },
    readAll,
    h('button', { onclick: () => invoke('open_settings') }, '설정'),
    h('button', { class: 'primary', onclick: () => invoke('open_picker', { files: [] }) }, '새 쪽지')));
}

async function sent() {
  const s = await invoke('get_sent', { id: V.id });
  if (!s) return close();
  const failed = s.to.filter((r) => !r.ok);
  render(
    h('div', { class: 'head' }, h('b', {}, '→ ' + s.to.map((r) => r.name).join(', ')), h('span', { class: 'muted' }, time(s.ts))),
    h('div', { class: 'body grow' }, linkify(s.text)),
    s.files.length && h('div', { class: 'files' }, s.files.map((f) =>
      h('div', { class: 'file' },
        h('a', { title: f.path, onclick: () => invoke('open_path', { path: f.path, reveal: false }) }, f.name),
        h('span', { class: 'muted' }, size(f.size))))),
    failed.length && h('div', { class: 'err' }, failed.map((r) => `전송 실패 - ${r.name}: ${r.error}`).join('\n')),
    h('div', { class: 'bar' }, h('button', { onclick: close }, '닫기')),
  );
}

// ---------------------------------------------------------------- receiving
function recv() {
  const prog = h('progress', { max: 1, value: 0 });
  const info = h('div', { class: 'muted' }, `0 / ${size(V.total)}`);
  listen('recv-progress', ({ payload: p }) => {
    if (p.id !== V.id) return;
    prog.value = p.total ? p.done / p.total : 0;
    info.textContent = `${size(p.done)} / ${size(p.total)} · ${p.file}`;
  });
  render(
    h('div', {}, h('b', {}, V.from), ` 님이 파일 ${V.count}개를 보내는 중입니다`),
    prog, info,
    h('div', { class: 'bar' }, h('button', { onclick: () => { invoke('cancel_recv', { id: V.id }); close(); } }, '수신 취소')));
}

// ---------------------------------------------------------------- settings
async function settings() {
  const s = await invoke('get_settings');
  const me = await invoke('me');
  const field = (label, input) => h('label', {}, h('span', { class: 'muted' }, label), input);
  const check = (label, input) => h('label', { class: 'check' }, input, label);
  const f = {
    name: h('input', { value: s.name }),
    notify: h('input', { type: 'checkbox', checked: s.notify }),
    autostart: h('input', { type: 'checkbox', checked: s.autostart }),
    context_menu: h('input', { type: 'checkbox', checked: s.context_menu }),
    max_files: h('input', { type: 'number', min: 0, value: s.max_files }),
    max_file_gb: h('input', { type: 'number', min: 0, step: 'any', value: s.max_file_gb }),
    download_dir: h('input', { value: s.download_dir }),
    signature: h('textarea', { rows: 3, value: s.signature, placeholder: '예: 홍길동 / 개발팀 내선 1234' }),
    extra_hosts: h('input', { value: s.extra_hosts, placeholder: '예: 10.0.1.23, 10.0.2.5:55001' }),
    scan_span: h('input', { type: 'number', min: 1, value: s.scan_span }),
    efficient: h('input', { type: 'radio', name: 'mode', checked: !s.perf_mode }),
    perf_mode: h('input', { type: 'radio', name: 'mode', checked: s.perf_mode }),
  };
  const err = h('div', { class: 'err' });
  async function save(e) {
    e.preventDefault();
    const n = {
      ...s,
      name: f.name.value,
      notify: f.notify.checked,
      autostart: f.autostart.checked,
      context_menu: f.context_menu.checked,
      max_files: Math.max(0, parseInt(f.max_files.value, 10) || 0),
      max_file_gb: Math.max(0, parseFloat(f.max_file_gb.value) || 0),
      download_dir: f.download_dir.value,
      extra_hosts: f.extra_hosts.value,
      signature: f.signature.value,
      scan_span: Math.max(1, parseInt(f.scan_span.value, 10) || 10),
      perf_mode: f.perf_mode.checked,
    };
    try { await invoke('save_settings', { s: n }); close(); } catch (x) { err.textContent = String(x); }
  }
  render(h('form', { class: 'grow', onsubmit: save },
    field('이름', f.name),
    field('서명 (새 쪽지와 답장에 자동으로 들어감)', f.signature),
    check('메시지가 오면 팝업으로 표시 (끄면 트레이에 보관)', f.notify),
    check('시스템 시작 시 실행', f.autostart),
    check('탐색기 우클릭 메뉴 "Send using PopupMessenger" (Windows)', f.context_menu),
    h('div', { class: 'inline' }, field('받을 첨부파일 수', f.max_files), field('파일당 최대 크기 (GB)', f.max_file_gb)),
    field('받은 파일 저장 위치', h('div', { class: 'inline' }, f.download_dir,
      h('button', { type: 'button', onclick: async () => { const d = await invoke('pick_dir'); if (d) f.download_dir.value = d; } }, '…'))),
    field('추가 탐색 주소 (브로드캐스트가 닿지 않는 PC)', f.extra_hosts),
    field('탐색 포트 수 (55000부터)', f.scan_span),
    h('fieldset', {}, h('legend', { class: 'muted' }, '메모리 절약'),
      check(h('span', {}, '효율 모드 ', h('span', { class: 'muted' }, '— 창을 닫으면 메모리를 바로 반환. 느린 PC에선 창이 잠깐 하얗게 보일 수 있음')), f.efficient),
      check(h('span', {}, '성능 모드 ', h('span', { class: 'muted' }, '— 창 하나를 미리 그려 두어 바로 표시. 메모리를 더 사용')), f.perf_mode)),
    h('div', { class: 'muted' }, `내 포트 ${me.port} · 디스크 여유 ${size(me.meta.free_space)} · 실제 허용 ${size(me.meta.max_file_size)}/파일`),
    err,
    h('div', { class: 'bar' }, h('button', { type: 'button', onclick: close }, '취소'), h('button', { class: 'primary' }, '저장'))));
}

// ---------------------------------------------------------------- boot
const views = { popup, sent, compose, pick, inbox, recv, settings };

// Renders first and only then shows the window, so a pre-warmed (spare) window
// never flashes blank. Showing an already visible window is a no-op.
async function boot(v) {
  V = v;
  try { await (views[v.v] || inbox)(); } finally { win.show().then(() => win.setFocus()).catch(() => {}); }
}

if (window.__SPARE) {
  window.__boot = boot;
  invoke('spare_ready');
} else {
  boot(window.__VIEW || { v: 'inbox' });
}
