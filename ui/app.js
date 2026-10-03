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
const MAC = navigator.userAgent.includes('Mac');
const SEND_KEY = MAC ? '⌘+Enter' : 'Ctrl+Enter';
const isSendKey = (e) => e.key === 'Enter' && (e.ctrlKey || e.metaKey);
const close = () => win.close();
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input,textarea,.body')) e.preventDefault(); });

// File drops. macOS: Tauri's native handler delivers paths (onDragDropEvent).
// Windows: WebView2 receives the drop itself and the files are forwarded to the
// app, which answers with their paths via window.__pmDrop (see src-tauri/src/windrop.rs).
// Views that accept files set onDrop; everywhere else drops are refused, which also
// keeps WebView2 from navigating to a dropped file.
let onDrop = null;
const hasFiles = (e) => e.dataTransfer && [...e.dataTransfer.types].includes('Files');
const highlight = (on) => $app.classList.toggle('drop', on && !!onDrop);
document.addEventListener('dragover', (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = onDrop ? 'copy' : 'none';
  highlight(true);
});
document.addEventListener('dragleave', (e) => { if (!e.relatedTarget) highlight(false); });
document.addEventListener('drop', (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  highlight(false);
  if (onDrop && e.dataTransfer.files.length && window.chrome?.webview) {
    window.chrome.webview.postMessageWithAdditionalObjects('pm-drop', e.dataTransfer.files);
  }
});
window.__pmDrop = (paths) => { if (onDrop && paths.length) onDrop(paths); };
T.webview.getCurrentWebview().onDragDropEvent(({ payload: p }) => {
  highlight(p.type === 'enter' || p.type === 'over');
  if (p.type === 'drop' && onDrop) onDrop(p.paths);
});

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
  const myId = m.group ? (await invoke('me')).meta.id : '';
  const others = m.group ? m.group.members.filter((x) => x.id !== myId) : [];
  const replyAll = () => { invoke('open_compose', { to: [m.from_id, ...others.map((x) => x.id)], files: [], reply: m.id }); close(); };
  render(
    h('div', { class: 'head' }, h('b', {}, m.from_name), h('span', { class: 'muted' }, `${m.from_ip} · ${time(m.ts)}`)),
    m.group && groupInfo(m.group, others),
    h('div', { class: 'body grow' }, linkify(m.text)),
    m.files.length && h('div', { class: 'files' }, m.files.map((f) =>
      h('div', { class: 'file' },
        h('a', { title: f.path, onclick: () => invoke('open_path', { path: f.path, reveal: false }) }, f.name),
        h('span', { class: 'muted' }, size(f.size)),
        h('a', { class: 'muted', onclick: () => invoke('open_path', { path: f.path, reveal: true }) }, '폴더')))),
    m.note && h('div', { class: 'err' }, m.note),
    h('div', { class: 'bar' },
      others.length && h('button', { onclick: replyAll }, '전체 답장'),
      h('button', { class: 'primary', onclick: reply }, '답장'),
      h('button', { onclick: close }, '닫기')),
  );
  document.addEventListener('keydown', (e) => { if (isSendKey(e)) reply(); });
}

// Group name and the other recipients of a message sent to a revealed group.
function groupInfo(g, others) {
  return h('details', { class: 'group' },
    h('summary', {}, h('span', { class: 'muted' }, '그룹 '), h('b', {}, g.name), h('span', { class: 'muted' }, ` · 함께 받은 사람 ${others.length}명`)),
    h('div', { class: 'members' }, others.length ? others.map((x) => x.name || x.id).join(', ') : '(없음)'));
}

// ---------------------------------------------------------------- compose
async function compose() {
  const peers = await invoke('peers');
  let to = V.to.slice();
  let files = V.files.slice();
  const nameOf = (id) => (peers.find((p) => p.id === id) || { display: id }).display;
  // Group mode: recipients are the group's online members (offline ones are listed as skipped).
  let group = V.group ? (await invoke('groups')).find((g) => g.id === V.group) : null;
  if (V.group && !group) return close();
  let tried = false; // after a send attempt, `to` holds only the failures
  const groupTo = () => group.members.filter((x) => x.online || x.relay).map((x) => x.id);
  if (group) to = groupTo();
  const reveal = group && h('input', { type: 'checkbox', checked: group.reveal });
  const { signature } = await invoke('get_settings');
  const orig = V.reply ? await invoke('get_message', { id: V.reply }) : null;
  // Cursor goes on top; signature and the quoted original sit below three blank lines.
  const below = [signature.trimEnd(), orig && quote(orig)].filter(Boolean).join('\n\n');
  const initial = below ? `\n\n\n${below}` : '';

  const toLine = h('div', { class: 'muted' });
  const dndLine = h('div', { class: 'warn', hidden: true });
  const relayLine = h('div', { class: 'muted', hidden: true });
  const stats = new Map(); // path -> { dir, size, ok }
  const ta = h('textarea', { class: 'grow', placeholder: `내용 (${SEND_KEY} 전송)`, value: initial });
  const list = h('div', { class: 'files' });
  const prog = h('progress', { max: 1, value: 0, hidden: true });
  const status = h('span', { class: 'status muted' });
  const sendBtn = h('button', { class: 'primary', onclick: send }, '전송');

  const drawTo = () => {
    toLine.textContent = `받는 사람: ${to.map(nameOf).join(', ')}`;
    if (group) {
      const off = group.members.filter((x) => !x.online && !x.relay).map((x) => x.display);
      toLine.replaceChildren(h('b', {}, `[그룹] ${group.name}`), ` → ${to.map(nameOf).join(', ') || '(보낼 수 있는 사람 없음)'}`,
        off.length ? h('div', {}, `오프라인이라 제외: ${off.join(', ')}`) : '');
    }
    const stored = to.filter((id) => { const p = peers.find((x) => x.id === id); return p && !p.online && p.reach; }).map(nameOf);
    relayLine.hidden = !stored.length;
    relayLine.textContent = `📮 ${stored.join(', ')} 님은 오프라인입니다. 릴레이 서버에 보관했다가 접속하면 전달합니다.`;
    const dnd = to.filter((id) => (peers.find((p) => p.id === id) || {}).status === 'dnd').map(nameOf);
    dndLine.hidden = !dnd.length;
    dndLine.textContent = `⛔ ${dnd.join(', ')} 님은 방해금지 상태입니다. 알림 없이 쪽지함으로 전달됩니다.`;
  };
  const drawFiles = () => list.replaceChildren(...(files.length ? [] : [h('div', { class: 'muted hint' }, '파일이나 폴더를 이 창에 끌어다 놓으면 첨부됩니다 (폴더는 zip으로 압축)')]), ...files.map((f, i) => {
    const s = stats.get(f);
    const info = !s ? '' : !s.ok ? '읽을 수 없음' : s.dir ? '폴더 → zip으로 압축' : size(s.size);
    return h('div', { class: 'file' },
      h('span', { title: f }, base(f) + (s && s.dir ? '/' : '')),
      h('span', { class: s && !s.ok ? 'err' : 'muted' }, info),
      h('a', { class: 'muted', onclick: () => { files.splice(i, 1); drawFiles(); } }, '✕'));
  }));
  const addFiles = async (ps) => {
    files = [...new Set([...files, ...ps])];
    drawFiles();
    for (const s of await invoke('stat_paths', { paths: files.filter((f) => !stats.has(f)) })) stats.set(s.path, s);
    drawFiles();
  };

  async function send() {
    if (sendBtn.disabled || !to.length || ((!ta.value.trim() || ta.value === initial) && !files.length)) return;
    sendBtn.disabled = true;
    status.className = 'status muted';
    status.textContent = '보내는 중…';
    const job = Math.random().toString(36).slice(2);
    const un = await listen('send-progress', ({ payload: p }) => {
      if (p.job !== job) return;
      if (p.zip) { status.textContent = `'${p.zip}' 압축 중…`; return; }
      prog.hidden = false;
      prog.value = p.total ? p.done / p.total : 0;
      status.textContent = `${p.to}: ${size(p.done)} / ${size(p.total)}`;
    });
    const res = await invoke('send_message', { job, to, text: ta.value, files, group: group ? group.id : null, reveal: !!(reveal && reveal.checked) });
    un();
    tried = true;
    const failed = res.filter((r) => !r.ok);
    if (!failed.length) return close();
    to = failed.map((r) => r.to); // retry only the failures
    drawTo();
    prog.hidden = true;
    status.className = 'status err';
    status.textContent = failed.map((r) => `${r.name}: ${r.error}`).join('\n');
    sendBtn.disabled = false;
  }

  ta.addEventListener('keydown', (e) => { if (isSendKey(e)) send(); });
  onDrop = addFiles;
  drawTo();
  addFiles([]);
  listen('peers', async () => {
    peers.splice(0, peers.length, ...(await invoke('peers')));
    if (group && !tried) {
      group = (await invoke('groups')).find((g) => g.id === group.id) || group;
      to = groupTo();
    }
    drawTo();
  });
  render(toLine, dndLine, relayLine,
    reveal && h('label', { class: 'check', title: '그룹 기능을 지원하는 버전(0.1.4 이상)에서만 보입니다' }, reveal, '받는 사람에게 그룹 이름과 다른 받는 사람 공개'),
    ta, list, prog,
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
    const shown = peers.filter((p) => p.reach && (!s || `${p.display} ${p.host} ${p.ip}`.toLowerCase().includes(s)));
    list.replaceChildren(...(shown.length ? shown.map((p) => {
      const cb = h('input', { type: 'checkbox', checked: sel.has(p.id), onchange: () => {
        cb.checked ? sel.add(p.id) : sel.delete(p.id);
        go.disabled = !sel.size;
        go.textContent = sel.size ? `${sel.size}명에게 쓰기` : '쓰기';
      } });
      return h('div', { class: 'row' + (p.online ? '' : ' off') }, cb,
        h('span', { class: 'name', title: '클릭하면 바로 쓰기', onclick: () => open([p.id]) }, p.display),
        p.status === 'dnd' && h('span', { class: 'warn', title: '알림 없이 쪽지함으로 전달됩니다' }, '방해금지'),
        !p.online && h('span', { class: 'muted', title: '릴레이 서버에 보관했다가 접속하면 전달합니다' }, '보관 전달'),
        p.relay && p.online && h('span', { class: 'muted', title: '릴레이 서버를 거쳐 전달합니다' }, '릴레이'),
        p.host && h('span', { class: 'muted' }, p.host));
    }) : [h('div', { class: 'empty muted' }, '보낼 수 있는 사용자가 없습니다')]));
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
        h('span', { class: 'preview' }, (m.group ? `[${m.group.name}] ` : '') + preview(m.text, m.files)),
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
        h('span', { class: 'name', title: s.to.map((r) => r.name).join(', ') }, '→ ' + (s.group ? `[${s.group}] ` : '') + s.to.map((r) => r.name).join(', ')),
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
    h('div', { class: 'head' }, h('b', {}, '→ ' + (s.group ? `[${s.group}] ` : '') + s.to.map((r) => r.name).join(', ')), h('span', { class: 'muted' }, time(s.ts))),
    h('div', { class: 'body grow' }, linkify(s.text)),
    s.files.length && h('div', { class: 'files' }, s.files.map((f) =>
      h('div', { class: 'file' },
        h('a', { title: f.path, onclick: () => invoke('open_path', { path: f.path, reveal: false }) }, f.name),
        h('span', { class: 'muted' }, size(f.size))))),
    s.to.some((r) => r.ok && r.via && r.via !== 'lan') && h('div', { class: 'muted' },
      s.to.filter((r) => r.ok && r.via && r.via !== 'lan').map((r) => `${r.name}: ${r.via === 'stored' ? '릴레이 서버에 보관 (접속하면 전달)' : '릴레이로 전달'}`).join('\n')),
    failed.length && h('div', { class: 'err' }, failed.map((r) => `전송 실패 - ${r.name}: ${r.error}`).join('\n')),
    h('div', { class: 'bar' }, h('button', { onclick: close }, '닫기')),
  );
}

// ---------------------------------------------------------------- send groups
async function groups() {
  const list = h('div', { class: 'grow' });
  const err = h('div', { class: 'err' });
  async function drawList() {
    const gs = await invoke('groups');
    err.textContent = '';
    list.replaceChildren(...(gs.length ? gs.map((g) =>
      h('div', { class: 'row' + (g.online ? '' : ' off') },
        h('span', { class: 'name', title: g.members.map((x) => x.display).join(', '), onclick: () => edit(g) }, g.name),
        h('span', { class: 'muted' }, `${g.online}/${g.members.length}명`),
        h('a', { onclick: () => invoke('open_group_compose', { id: g.id }) }, '보내기'),
        h('a', { onclick: () => edit(g) }, '편집'),
        h('a', { class: 'muted', title: '삭제', onclick: () => { if (confirm(`'${g.name}' 그룹을 삭제할까요?`)) invoke('delete_group', { id: g.id }); } }, '✕')))
      : [h('div', { class: 'empty muted' }, '전송 그룹이 없습니다')]));
    render(list, err, h('div', { class: 'bar' },
      h('button', { onclick: close }, '닫기'),
      h('button', { class: 'primary', onclick: () => edit(null) }, '새 그룹')));
  }
  // Every known user can be picked, including offline ones.
  async function edit(g) {
    const peers = await invoke('peers');
    const sel = new Set(g ? g.members.map((x) => x.id) : []);
    const name = h('input', { value: g ? g.name : '', placeholder: '그룹 이름' });
    const reveal = h('input', { type: 'checkbox', checked: g ? g.reveal : true });
    const q = h('input', { placeholder: '이름 / 호스트 / IP 검색', oninput: draw });
    const box = h('div', { class: 'grow' });
    const count = h('span', { class: 'status muted' });
    // Members missing from the peer list stay selected and listed.
    const extra = g ? g.members.filter((x) => !peers.some((p) => p.id === x.id)).map((x) => ({ id: x.id, display: x.display, host: '', ip: '', online: false })) : [];
    function draw() {
      const s = q.value.trim().toLowerCase();
      const shown = [...peers, ...extra].filter((p) => !s || `${p.display} ${p.host} ${p.ip}`.toLowerCase().includes(s));
      box.replaceChildren(...(shown.length ? shown.map((p) => {
        const cb = h('input', { type: 'checkbox', checked: sel.has(p.id), onchange: () => { cb.checked ? sel.add(p.id) : sel.delete(p.id); counted(); } });
        return h('label', { class: 'check row' + (p.online ? '' : ' off') }, cb,
          h('span', { class: 'name' }, p.display),
          !p.online && h('span', { class: 'muted' }, '오프라인'),
          p.host && h('span', { class: 'muted' }, p.host));
      }) : [h('div', { class: 'empty muted' }, '아는 사용자가 없습니다')]));
    }
    const counted = () => { count.textContent = `${sel.size}명 선택`; };
    async function save() {
      try {
        await invoke('save_group', { g: { id: g ? g.id : '', name: name.value, members: [...sel], reveal: reveal.checked } });
        drawList();
      } catch (x) { err.textContent = String(x); }
    }
    name.addEventListener('keydown', (e) => { if (e.key === 'Enter') save(); });
    draw();
    counted();
    err.textContent = '';
    render(name, h('label', { class: 'check' }, reveal, '보낼 때 그룹 이름과 다른 받는 사람 공개 (기본값)'), q, box, err,
      h('div', { class: 'bar' }, count,
        h('button', { onclick: drawList }, '취소'),
        h('button', { class: 'primary', onclick: save }, g ? '저장' : '만들기')));
    name.focus();
  }
  listen('groups', () => { if (list.isConnected) drawList(); });
  listen('peers', () => { if (list.isConnected) drawList(); });
  await drawList();
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
    show_host: h('input', { type: 'checkbox', checked: s.show_host }),
    notify: h('input', { type: 'checkbox', checked: s.notify }),
    autostart: h('input', { type: 'checkbox', checked: s.autostart }),
    context_menu: h('input', { type: 'checkbox', checked: s.context_menu }),
    auto_online: h('input', { type: 'checkbox', checked: s.auto_online }),
    split_folders: h('input', { type: 'checkbox', checked: s.split_folders }),
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
  const relayBox = h('fieldset', { class: 'relay' });
  drawRelay(relayBox);
  listen('relay', () => drawRelay(relayBox));
  const nameWarn = h('div', { class: 'warn', hidden: true });
  f.name.addEventListener('input', async () => {
    const taken = await invoke('name_taken', { name: f.name.value });
    nameWarn.hidden = !taken;
    nameWarn.textContent = taken ? `같은 네트워크에 '${f.name.value.trim()}' 사용자가 이미 있습니다.` : '';
  });
  async function save(e) {
    e.preventDefault();
    const n = {
      ...s,
      name: f.name.value,
      show_host: f.show_host.checked,
      notify: f.notify.checked,
      autostart: f.autostart.checked,
      context_menu: f.context_menu.checked,
      auto_online: f.auto_online.checked,
      split_folders: f.split_folders.checked,
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
    nameWarn,
    field('서명 (새 쪽지와 답장에 자동으로 들어감)', f.signature),
    check('메시지가 오면 팝업으로 표시 (끄면 트레이에 보관)', f.notify),
    check('시스템 시작 시 실행', f.autostart),
    check('재시작 시 온라인으로 자동 전환', f.auto_online),
    check('컴퓨터 호스트 이름 보기 (끄면 이름이 같은 사람은 이름-xxxx로 구분)', f.show_host),
    check(me.os === 'macos'
      ? 'Finder 우클릭 > 빠른 동작 "Send using PopupMessenger" (파일·폴더)'
      : '탐색기 우클릭 메뉴 "Send using PopupMessenger" (파일·폴더)', f.context_menu),
    h('div', { class: 'inline' }, field('받을 첨부파일 수', f.max_files), field('파일당 최대 크기 (GB)', f.max_file_gb)),
    field('받은 파일 저장 위치', h('div', { class: 'inline' }, f.download_dir,
      h('button', { type: 'button', onclick: async () => { const d = await invoke('pick_dir'); if (d) f.download_dir.value = d; } }, '…'))),
    check('쪽지마다 폴더를 따로 만들어 저장 (보낸 사람 + 날짜)', f.split_folders),
    field('추가 탐색 주소 (브로드캐스트가 닿지 않는 PC)', f.extra_hosts),
    field('탐색 포트 수 (55000부터)', f.scan_span),
    h('fieldset', {}, h('legend', { class: 'muted' }, '메모리 절약'),
      check(h('span', {}, '효율 모드 ', h('span', { class: 'muted' }, '— 창을 닫으면 메모리를 바로 반환. 느린 PC에선 창이 잠깐 하얗게 보일 수 있음')), f.efficient),
      check(h('span', {}, '성능 모드 ', h('span', { class: 'muted' }, '— 창 하나를 미리 그려 두어 바로 표시. 메모리를 더 사용')), f.perf_mode)),
    h('div', { class: 'muted' }, `내 포트 ${me.port} · 디스크 여유 ${size(me.meta.free_space)} · 실제 허용 ${size(me.meta.max_file_size)}/파일`),
    relayBox,
    err,
    h('div', { class: 'bar' }, h('button', { type: 'button', onclick: close }, '취소'), h('button', { class: 'primary' }, '저장'))));
}

// ---------------------------------------------------------------- relay
const ENC = { plain: '평문 (암호화 없음)', tls: '서버 통신 암호화 (TLS)', e2ee: '종단간 암호화' };
const AUTH = { password: '사용자 이름 + 비밀번호', none: '계정 없음 (기기 단위 등록)' };
const REG = { closed: '닫힘 (관리자가 만든 계정만)', approval: '승인제', open: '열림' };
const CONN = { online: ['ok', '연결됨'], connecting: ['busy', '연결 중…'], pending: ['warn', '승인 대기'], error: ['err', '연결 안 됨'], off: ['muted', '꺼짐'] };
const fp = (hex) => (hex || '').toUpperCase().match(/.{1,4}/g)?.join(' ') || '';
const kv = (rows) => h('div', { class: 'kv' }, rows.filter(Boolean).flatMap(([k, v]) => [h('span', { class: 'muted' }, k), h('span', {}, v)]));
function policyRows(p) {
  return [
    ['암호화', ENC[p.encryption] || p.encryption],
    ['첨부 중계', p.relay_max_files ? `${p.relay_max_files}개 · 파일당 ${size(p.relay_max_file_size)}` : '하지 않음'],
    ['보관', `쪽지 ${p.retention_hours.text}시간 · 첨부 ${p.retention_hours.files}시간`],
    p.force_relay && ['경로', p.block_direct_outsiders ? '릴레이 경유 강제 · 서버 밖 사용자 직접 전송 금지' : '릴레이 경유 강제'],
  ];
}

// Settings panel section: account info, or how to link one.
async function drawRelay(box) {
  const r = await invoke('relay_info');
  const legend = h('legend', { class: 'muted' }, '릴레이 서버 계정');
  if (!r.configured) {
    box.replaceChildren(legend,
      h('div', { class: 'muted' }, '연결된 릴레이 서버가 없습니다. 릴레이 서버에 연결하면 다른 네트워크에 있거나 꺼져 있는 사람에게도 쪽지를 보낼 수 있습니다.'),
      h('div', { class: 'bar' }, h('button', { type: 'button', class: 'primary', onclick: () => invoke('open_relay_setup') }, '릴레이 서버 연결…')));
    return;
  }
  const [cls, label] = CONN[r.state] || CONN.off;
  const p = r.policy || {};
  const changed = r.key_changed || [];
  box.replaceChildren(legend,
    kv([
      ['서버', `${r.server_name || '(이름 없음)'} · ${r.url}`],
      ['상태', h('span', {}, h('span', { class: 'conn ' + cls }, '● ', label), r.message ? h('span', { class: 'muted' }, ` ${r.message}`) : '')],
      ['계정', r.username || '(계정 없음 — 기기 등록)'],
      ['계정 ID', h('span', { class: 'mono' }, r.account || '—')],
      ['기기 ID', h('span', { class: 'mono' }, r.device)],
      ['연결', h('span', {}, /^https?:\/\//i.test(r.url) ? 'WebSocket · ' : 'TCP · ',
        r.tls ? h('span', { title: r.fingerprint }, 'TLS · 인증서 ', h('span', { class: 'mono' }, fp(r.fingerprint).slice(0, 19))) : h('span', { class: 'warn' }, '⚠ 암호화되지 않은 연결'))],
      p.encryption && ['로그온', AUTH[p.auth] || p.auth],
      ...(p.encryption ? policyRows(p) : []),
      p.encryption === 'e2ee' && ['내 보안 키', h('span', { class: 'mono' }, fp(r.my_key))],
      ['연락처', `${r.contacts}명`],
    ]),
    changed.length ? h('div', { class: 'warn' },
      `보안 키가 바뀐 기기가 있습니다: ${changed.map((c) => `${c.name} (${fp(c.fingerprint)})`).join(', ')}. 상대에게 직접 확인한 뒤 수락하세요. 수락 전에는 이 사람들에게 종단간 암호화 쪽지를 보내지 않습니다.`,
      ' ', h('a', { onclick: () => invoke('relay_accept_keys') }, '새 키 수락')) : '',
    h('div', { class: 'bar' },
      h('button', { type: 'button', onclick: () => invoke('relay_reconnect') }, '다시 연결'),
      h('button', { type: 'button', onclick: async () => {
        if (confirm('릴레이 서버 연결을 해제할까요?\n릴레이 연락처가 목록에서 사라집니다. 같은 서버에 다시 연결하면 이 기기로 다시 로그온합니다.')) await invoke('relay_disconnect');
      } }, '연결 해제')));
}

// Setup window: server address -> confirm identity -> account -> done.
async function relaysetup() {
  let probe = null;
  const err = h('div', { class: 'err' });
  const busy = (btn, on, text) => { btn.disabled = on; if (text) btn.textContent = text; };
  const steps = ['서버', '확인', '계정', '완료'];
  const head = (i) => h('div', { class: 'steps' }, steps.map((t, j) => h('span', { class: j === i ? 'on' : j < i ? 'done' : '' }, `${j + 1}. ${t}`)));

  function stepAddr(value = '') {
    const url = h('input', { value, placeholder: '예: https://relay.example.com 또는 192.168.0.10:45443' });
    const next = h('button', { class: 'primary', onclick: go }, '다음');
    async function go() {
      err.textContent = '';
      if (!url.value.trim()) { url.focus(); return; }
      busy(next, true, '연결 중…');
      try { probe = await invoke('relay_probe', { url: url.value }); stepConfirm(); }
      catch (x) { err.textContent = String(x); busy(next, false, '다음'); }
    }
    url.addEventListener('keydown', (e) => { if (e.key === 'Enter') go(); });
    render(head(0),
      h('div', { class: 'grow form' },
        h('label', {}, h('span', { class: 'muted' }, '릴레이 서버 주소'), url),
        h('div', { class: 'muted' }, '관리자에게 받은 주소를 입력하세요. https://로 시작하는 주소는 WebSocket으로 연결합니다 (Cloudflare Tunnel 같은 HTTPS 프록시). 그 밖의 주소는 TCP로 직접 연결하며, 포트를 생략하면 45443을 씁니다.'),
        err),
      h('div', { class: 'bar' }, h('button', { onclick: close }, '취소'), next));
    url.focus();
  }

  function stepConfirm() {
    const s = probe.server || {}, p = probe.policy || {};
    render(head(1),
      h('div', { class: 'grow form' },
        kv([
          ['서버 이름', h('b', {}, s.name || '(이름 없음)')],
          ['주소', probe.url],
          ['연결 방식', probe.ws ? `WebSocket${probe.tls ? ' (HTTPS)' : ' (암호화 없음)'}` : `TCP${probe.tls ? ' · TLS' : ' (암호화 없음)'}`],
          ['서버 버전', s.version || '—'],
          ['로그온', AUTH[p.auth] || p.auth],
          ['가입', REG[p.registration] || p.registration],
          ...policyRows(p),
        ]),
        probe.tls
          ? (probe.trusted
            ? h('div', { class: 'muted' }, '🔒 공인 인증서로 확인된 서버입니다.')
            : h('div', { class: 'note' },
              h('div', {}, '🔒 자체 서명 인증서입니다. 아래 지문이 관리자가 알려 준 지문(관리 화면 > 개요)과 같은지 확인하세요. 같으면 이 서버를 신뢰합니다.'),
              h('div', { class: 'fp' }, fp(probe.fingerprint))))
          : h('div', { class: 'warn' }, '⚠ 이 서버는 암호화되지 않은 연결만 받습니다. 쪽지와 파일이 네트워크에 그대로 노출됩니다. 신뢰할 수 있는 내부망에서만 쓰세요.'),
        err),
      h('div', { class: 'bar' }, h('button', { onclick: () => stepAddr(probe.url) }, '이전'), h('button', { class: 'primary', onclick: () => stepAuth() }, '신뢰하고 계속')));
  }

  function stepAuth(tab = 'login') {
    err.textContent = '';
    const p = probe.policy || {};
    const join = async (btn, mode, user = '', pass = '') => {
      err.textContent = '';
      busy(btn, true, '처리 중…');
      try {
        const r = await invoke('relay_join', { url: probe.url, tls: probe.tls, fingerprint: probe.fingerprint, mode, user, pass });
        stepDone(r);
      } catch (x) { err.textContent = String(x); busy(btn, false, mode === 'register' ? '가입 신청' : mode === 'login' ? '로그인' : '이 기기 등록'); }
    };
    if (p.auth === 'none') {
      const btn = h('button', { class: 'primary', onclick: () => join(btn, 'device') }, '이 기기 등록');
      render(head(2),
        h('div', { class: 'grow form' },
          h('div', {}, '이 서버는 계정 없이 기기 단위로 등록합니다. 이름과 상태는 이 앱의 설정을 그대로 씁니다.'),
          p.registration === 'closed' && h('div', { class: 'warn' }, '이 서버는 새 기기의 가입을 받지 않습니다. 관리자가 미리 허용하지 않았다면 등록되지 않습니다.'),
          p.registration === 'approval' && h('div', { class: 'muted' }, '등록하면 관리자 승인 후에 연결됩니다.'),
          err),
        h('div', { class: 'bar' }, h('button', { onclick: () => stepConfirm() }, '이전'), btn));
      return;
    }
    const canRegister = p.registration !== 'closed';
    if (!canRegister) tab = 'login';
    const user = h('input', { autocomplete: 'username' });
    const pass = h('input', { type: 'password', autocomplete: tab === 'login' ? 'current-password' : 'new-password' });
    const pass2 = h('input', { type: 'password', autocomplete: 'new-password' });
    const btn = h('button', { class: 'primary' }, tab === 'login' ? '로그인' : '가입 신청');
    btn.onclick = () => {
      if (!user.value.trim() || !pass.value) { err.textContent = '사용자 이름과 비밀번호를 입력하세요'; return; }
      if (tab === 'register' && pass.value !== pass2.value) { err.textContent = '비밀번호가 서로 다릅니다'; return; }
      join(btn, tab, user.value, pass.value);
    };
    [user, pass, pass2].forEach((i) => i.addEventListener('keydown', (e) => { if (e.key === 'Enter') btn.onclick(); }));
    const tabBtn = (k, t) => h('button', { class: tab === k ? 'on' : '', disabled: k === 'register' && !canRegister, onclick: () => stepAuth(k) }, t);
    render(head(2),
      h('div', { class: 'tabs' }, tabBtn('login', '기존 계정으로 로그인'), tabBtn('register', '가입 신청')),
      h('div', { class: 'grow form' },
        h('label', {}, h('span', { class: 'muted' }, '사용자 이름'), user),
        h('label', {}, h('span', { class: 'muted' }, '비밀번호'), pass),
        tab === 'register' && h('label', {}, h('span', { class: 'muted' }, '비밀번호 확인'), pass2),
        !canRegister && h('div', { class: 'muted' }, '이 서버는 가입을 받지 않습니다. 계정은 관리자에게 요청하세요.'),
        tab === 'register' && p.registration === 'approval' && h('div', { class: 'muted' }, '가입 신청 후 관리자가 승인하면 자동으로 연결됩니다.'),
        h('div', { class: 'muted' }, `비밀번호는 이 PC에 저장하지 않습니다. 이 기기가 계정에 등록되면 다음부터는 자동으로 로그온합니다.${p.max_devices ? ` (계정당 기기 ${p.max_devices}대까지)` : ''}`),
        err),
      h('div', { class: 'bar' }, h('button', { onclick: () => stepConfirm() }, '이전'), btn));
    user.focus();
  }

  function stepDone(r) {
    const ok = r.state === 'online';
    render(head(3),
      h('div', { class: 'grow form' },
        h('div', { class: ok ? '' : 'warn' }, ok
          ? `✅ ${probe.server.name || '릴레이 서버'}에 연결했습니다. 이제 다른 네트워크에 있거나 꺼져 있는 사람에게도 쪽지를 보낼 수 있습니다.`
          : `⏳ ${r.message || '가입 신청이 접수되었습니다.'} 관리자가 승인하면 자동으로 연결됩니다.`),
        h('div', { class: 'muted' }, '연결 상태와 계정 정보는 설정 창 맨 아래 "릴레이 서버 계정"에서 볼 수 있습니다.')),
      h('div', { class: 'bar' }, h('button', { class: 'primary', onclick: close }, '닫기')));
  }

  stepAddr();
}

// ---------------------------------------------------------------- boot
const views = { popup, sent, compose, pick, inbox, recv, settings, groups, relaysetup };

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
