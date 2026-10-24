// Slipstream's window. Renders the download list and talks to the app
// through Tauri commands; the app pushes changes as `slipstream` events.
//
// Everything that comes from the network (file names, links, errors) is put
// on the page with textContent, never as HTML.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

const ICONS = {
  plus: '<path d="M12 5v14M5 12h14"/>',
  play: '<path d="M7 4.8v14.4a.8.8 0 0 0 1.2.7l11.3-7.2a.8.8 0 0 0 0-1.4L8.2 4.1a.8.8 0 0 0-1.2.7z"/>',
  pause: '<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>',
  playAll: '<path d="M3 6v12l7-6zM12 6v12l7-6z"/>',
  pauseAll: '<rect x="4" y="4" width="16" height="16" rx="3"/><path d="M10 9v6M14 9v6"/>',
  trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  settings: '<path d="M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1"/><circle cx="15" cy="6" r="2"/><circle cx="9" cy="12" r="2"/><circle cx="17" cy="18" r="2"/>',
  search: '<circle cx="11" cy="11" r="6"/><path d="M20 20l-4.5-4.5"/>',
  download: '<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>',
  open: '<path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.66 0l3-3a4 4 0 0 0-5.66-5.66l-1 1M14 10a4 4 0 0 0-5.66 0l-3 3a4 4 0 0 0 5.66 5.66l1-1"/>',
  all: '<path d="M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01"/>',
  active: '<circle cx="12" cy="12" r="9"/><path d="M12 7.5v9M8.5 13l3.5 3.5 3.5-3.5"/>',
  queued: '<path d="M4 6h11M4 12h7M4 18h6"/><circle cx="17" cy="15" r="4"/><path d="M17 13.2v1.8l1.2 1.2"/>',
  unfinished: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  finished: '<circle cx="12" cy="12" r="9"/><path d="M8 12.5l2.7 2.7L16.2 9.5"/>',
  video: '<rect x="3" y="6" width="13" height="12" rx="2"/><path d="M16 10.5l5-3v9l-5-3"/>',
  music: '<path d="M9 18V6l11-2v12"/><circle cx="6.5" cy="18" r="2.5"/><circle cx="17.5" cy="16" r="2.5"/>',
  documents: '<path d="M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 16h6"/>',
  compressed: '<rect x="4" y="3" width="16" height="18" rx="2"/><path d="M12 3v2M12 7v2M12 11v2"/><rect x="10.5" y="14" width="3" height="4" rx="1"/>',
  programs: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M6.5 6.5h.01M9.5 6.5h.01"/>',
  images: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="2"/><path d="M21 16l-5-5-9 9"/>',
  other: '<path d="M6 3h8l4 4v14H6zM14 3v4h4"/>',
};
const svg = (name) => `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] ?? ''}</svg>`;

const STATUS_LABEL = {
  downloading: 'Downloading',
  queued: 'Queued',
  paused: 'Paused',
  completed: 'Completed',
  failed: 'Failed',
};
const STATUS_ORDER = { downloading: 0, queued: 1, paused: 2, failed: 3, completed: 4 };
const CATEGORIES = [
  ['video', 'Video'],
  ['music', 'Music'],
  ['documents', 'Documents'],
  ['compressed', 'Compressed'],
  ['programs', 'Programs'],
  ['images', 'Images'],
  ['other', 'Other'],
];
const VIEWS = [
  { id: 'all', label: 'All downloads', icon: 'all', test: () => true },
  { id: 'downloading', label: 'Downloading', icon: 'active', test: (i) => i.status === 'downloading' },
  { id: 'queued', label: 'Queued', icon: 'queued', test: (i) => i.status === 'queued' },
  { id: 'unfinished', label: 'Unfinished', icon: 'unfinished', test: (i) => i.status !== 'completed' },
  { id: 'finished', label: 'Finished', icon: 'finished', test: (i) => i.status === 'completed' },
  ...CATEGORIES.map(([id, label]) => ({ id: `cat:${id}`, label, icon: id, category: true, test: (i) => i.category === id })),
];

const state = {
  items: new Map(),
  live: new Map(),
  settings: null,
  totalSpeed: 0,
  view: 'all',
  search: '',
  sort: { key: 'added', dir: -1 },
  selected: new Set(),
  anchor: null,
};
const rows = new Map();

const canResume = (i) => i.status === 'paused' || i.status === 'failed';
const canPause = (i) => i.status === 'downloading' || i.status === 'queued';

// ---------- formatting ----------

function fmtBytes(n) {
  if (n == null) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${n.toFixed(n < 10 ? 2 : n < 100 ? 1 : 0)} ${units[i]}`;
}

const fmtSpeed = (bytesPerSec) => (bytesPerSec >= 1 ? `${fmtBytes(Math.round(bytesPerSec))}/s` : '');

function fmtEta(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '';
  const s = Math.round(seconds);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  return h > 99 ? '> 99h' : `${h}h ${Math.floor((s % 3600) / 60)}m`;
}

const dayFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
function fmtAdded(ms) {
  const d = new Date(ms);
  return d.toDateString() === new Date().toDateString() ? timeFmt.format(d) : dayFmt.format(d);
}

const percent = (i) => (i.total > 0 ? Math.min(100, Math.floor((i.downloaded / i.total) * 100)) : null);

function statusText(item) {
  const pct = percent(item);
  switch (item.status) {
    case 'downloading':
      if (pct != null) return `${pct}%`;
      return item.downloaded > 0 ? fmtBytes(item.downloaded) : 'Connecting…';
    case 'paused':
      return pct == null ? 'Paused' : `Paused · ${pct}%`;
    default:
      return STATUS_LABEL[item.status];
  }
}

function folderFor(item) {
  const s = state.settings;
  if (!s) return '';
  const sep = s.downloadDir.includes('\\') ? '\\' : '/';
  const sub = CATEGORIES.find(([id]) => id === item.category)?.[1];
  return s.useCategories && item.category !== 'other' ? `${s.downloadDir}${sep}${sub}` : s.downloadDir;
}

const setText = (el, text) => {
  if (el.textContent !== text) el.textContent = text;
};

// ---------- rendering ----------

let renderPending = false;
function render() {
  if (renderPending) return;
  renderPending = true;
  requestAnimationFrame(() => {
    renderPending = false;
    renderNow();
  });
}

function currentView() {
  return VIEWS.find((v) => v.id === state.view) ?? VIEWS[0];
}

function visibleItems() {
  const view = currentView();
  const q = state.search.trim().toLowerCase();
  const list = [...state.items.values()].filter(
    (i) => view.test(i) && (!q || i.fileName.toLowerCase().includes(q) || i.url.toLowerCase().includes(q)),
  );
  const key = {
    name: (i) => i.fileName.toLowerCase(),
    size: (i) => i.total ?? -1,
    status: (i) => STATUS_ORDER[i.status],
    added: (i) => i.addedAt,
  }[state.sort.key];
  list.sort((a, b) => {
    const x = key(a);
    const y = key(b);
    return (x < y ? -1 : x > y ? 1 : a.id - b.id) * state.sort.dir;
  });
  return list;
}

function selectedItems() {
  return [...state.selected].map((id) => state.items.get(id)).filter(Boolean);
}

function renderNow() {
  for (const id of state.selected) if (!state.items.has(id)) state.selected.delete(id);
  for (const id of rows.keys()) if (!state.items.has(id)) rows.delete(id);

  const list = visibleItems();
  const tbody = $('rows');
  for (const item of list) updateRow(rows.get(item.id) ?? createRow(item.id), item);
  const shown = [...tbody.children].map((tr) => Number(tr.dataset.id));
  if (shown.length !== list.length || list.some((item, k) => item.id !== shown[k])) {
    tbody.replaceChildren(...list.map((item) => rows.get(item.id)));
  }

  renderEmpty(list.length);
  renderSidebar();
  renderToolbar();
  renderDetails();
  renderStatusbar();
  renderSortHeaders();
}

function createRow(id) {
  const tr = document.createElement('tr');
  tr.dataset.id = id;
  tr.innerHTML =
    '<td><div class="name-cell"><span class="cat"></span><span class="fname"></span></div></td>' +
    '<td class="num size"></td>' +
    '<td><div class="prog"><div class="bar"><i></i></div><span class="ptext"></span></div></td>' +
    '<td class="num speed"></td><td class="num eta"></td><td class="num added"></td>';
  rows.set(id, tr);
  return tr;
}

function updateRow(tr, item) {
  const live = state.live.get(item.id);
  tr.className = `st-${item.status}${state.selected.has(item.id) ? ' selected' : ''}`;
  const cat = tr.querySelector('.cat');
  if (cat.dataset.cat !== item.category) {
    cat.dataset.cat = item.category;
    cat.className = `cat cat-${item.category}`;
    cat.innerHTML = svg(item.category);
  }
  const name = tr.querySelector('.fname');
  setText(name, item.fileName);
  name.title = item.fileName;
  setText(tr.querySelector('.size'), fmtBytes(item.total ?? (item.downloaded || null)));
  const pct = item.status === 'completed' ? 100 : percent(item) ?? 0;
  tr.querySelector('.bar i').style.width = `${pct}%`;
  const ptext = tr.querySelector('.ptext');
  setText(ptext, statusText(item));
  ptext.title = item.error ?? '';
  const speed = item.status === 'downloading' && live ? live.speed : 0;
  setText(tr.querySelector('.speed'), fmtSpeed(speed));
  const eta = speed > 0 && item.total ? (item.total - item.downloaded) / speed : NaN;
  setText(tr.querySelector('.eta'), fmtEta(eta));
  setText(tr.querySelector('.added'), fmtAdded(item.addedAt));
}

function renderEmpty(visibleCount) {
  const empty = $('empty');
  empty.hidden = visibleCount > 0;
  if (visibleCount > 0) return;
  if (state.items.size === 0) {
    setText($('empty-title'), 'No downloads yet');
    $('empty-hint').hidden = false;
  } else {
    setText($('empty-title'), state.search ? `Nothing matches “${state.search}”` : `Nothing in ${currentView().label}`);
    $('empty-hint').hidden = true;
  }
}

function renderSidebar() {
  const nav = $('sidebar');
  if (!nav.childElementCount) {
    const heading = (text) => {
      const h = document.createElement('h3');
      h.textContent = text;
      return h;
    };
    nav.append(heading('Status'));
    for (const view of VIEWS) {
      if (view.id === 'cat:video') nav.append(heading('Categories'));
      const button = document.createElement('button');
      button.dataset.view = view.id;
      button.innerHTML = `${svg(view.icon)}<span class="label"></span><span class="count"></span>`;
      if (view.category) button.querySelector('.ic').classList.add(`cat-${view.id.slice(4)}`);
      button.querySelector('.label').textContent = view.label;
      button.addEventListener('click', () => {
        state.view = view.id;
        render();
      });
      nav.append(button);
    }
  }
  const items = [...state.items.values()];
  for (const button of nav.querySelectorAll('button')) {
    const view = VIEWS.find((v) => v.id === button.dataset.view);
    button.classList.toggle('active', view.id === state.view);
    const count = items.filter(view.test).length;
    setText(button.querySelector('.count'), count ? String(count) : '');
  }
}

function renderToolbar() {
  const selected = selectedItems();
  const all = [...state.items.values()];
  $('btn-resume').disabled = !selected.some(canResume);
  $('btn-pause').disabled = !selected.some(canPause);
  $('btn-remove').disabled = selected.length === 0;
  $('btn-resume-all').disabled = !all.some(canResume);
  $('btn-pause-all').disabled = !all.some(canPause);
}

function renderSortHeaders() {
  for (const th of document.querySelectorAll('th[data-sort]')) {
    const active = th.dataset.sort === state.sort.key;
    th.classList.toggle('sorted', active);
    th.classList.toggle('asc', active && state.sort.dir > 0);
  }
}

function renderDetails() {
  const selected = selectedItems();
  const panel = $('details');
  panel.hidden = selected.length !== 1;
  if (panel.hidden) return;
  const item = selected[0];
  const live = state.live.get(item.id);

  const icon = $('d-icon');
  if (icon.dataset.cat !== item.category) {
    icon.dataset.cat = item.category;
    icon.className = `d-icon cat cat-${item.category}`;
    icon.innerHTML = svg(item.category);
  }
  setText($('d-name'), item.fileName);
  const pill = $('d-status');
  pill.className = `pill st-${item.status}`;
  setText(pill, STATUS_LABEL[item.status]);

  setText($('d-url'), item.url);
  $('d-url').title = item.url;
  const path = item.path ?? `${folderFor(item)} (when it starts)`;
  setText($('d-path'), path);
  $('d-path').title = path;

  let progress = item.total
    ? `${fmtBytes(item.downloaded)} of ${fmtBytes(item.total)} (${item.status === 'completed' ? 100 : percent(item)}%)`
    : fmtBytes(item.downloaded) || 'Not started';
  if (live?.speed >= 1) {
    progress += ` · ${fmtSpeed(live.speed)}`;
    const eta = item.total ? fmtEta((item.total - item.downloaded) / live.speed) : '';
    if (eta) progress += ` · ${eta} left`;
  }
  setText($('d-progress'), progress);
  setText($('d-conns'), live ? `${live.connections} active` : '—');
  setText(
    $('d-resume'),
    item.resumable == null
      ? 'Not known until it starts'
      : item.resumable
        ? 'Supported: pausing keeps your progress'
        : 'Not supported by this server: a paused download starts over',
  );
  for (const el of panel.querySelectorAll('.d-error')) el.hidden = !item.error;
  setText($('d-error'), item.error ?? '');
  drawSegmentMap(item, live);
}

/** IDM-style bar: filled where data has arrived, a marker where each piece starts. */
function drawSegmentMap(item, live) {
  const canvas = $('d-map');
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
  const h = Math.round(14 * dpr);
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const ctx = canvas.getContext('2d');
  const css = getComputedStyle(document.documentElement);
  const color = (name) => css.getPropertyValue(name).trim();

  ctx.fillStyle = color('--track');
  ctx.fillRect(0, 0, w, h);
  const fill = { completed: '--bar-done', paused: '--bar-paused', failed: '--err', queued: '--faint' }[item.status] ?? '--accent';
  ctx.fillStyle = color(fill);
  if (item.status === 'completed') {
    ctx.fillRect(0, 0, w, h);
    return;
  }
  const total = live?.total ?? item.total;
  if (!total) return;
  const x = (bytes) => (bytes / total) * w;
  if (live?.segments?.length) {
    for (const [start, , written] of live.segments) ctx.fillRect(x(start), 0, Math.max(0, x(written) - x(start)), h);
    ctx.fillStyle = color('--marker');
    for (const [start] of live.segments) if (start > 0) ctx.fillRect(Math.round(x(start)), 0, Math.max(1, dpr), h);
  } else {
    ctx.fillRect(0, 0, x(item.downloaded), h);
  }
}

function renderStatusbar() {
  const all = [...state.items.values()];
  const count = (status) => all.filter((i) => i.status === status).length;
  setText($('sb-speed'), state.totalSpeed >= 1 ? `↓ ${fmtSpeed(state.totalSpeed)}` : '');
  const parts = [
    [count('downloading'), 'downloading'],
    [count('queued'), 'queued'],
    [count('paused'), 'paused'],
    [count('failed'), 'failed'],
    [count('completed'), 'finished'],
  ]
    .filter(([n]) => n > 0)
    .map(([n, label]) => `${n} ${label}`);
  setText($('sb-counts'), parts.join(' · ') || 'No downloads');
  const limit = state.settings?.speedLimit ?? 0;
  setText($('sb-limit'), limit > 0 ? `Speed limit ${fmtSpeed(limit)}` : 'No speed limit');
}

// ---------- actions ----------

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (err) {
    toast(typeof err === 'string' ? err : err?.message ?? String(err), true);
    return undefined;
  }
}

const resumeSelected = () => call('resume', { ids: selectedItems().filter(canResume).map((i) => i.id) });
const pauseSelected = () => call('pause', { ids: selectedItems().filter(canPause).map((i) => i.id) });

function defaultAction(item) {
  if (item.status === 'completed') call('open_file', { id: item.id });
  else if (canResume(item)) call('resume', { ids: [item.id] });
}

async function copyLinks(items) {
  try {
    await navigator.clipboard.writeText(items.map((i) => i.url).join('\n'));
    toast(items.length > 1 ? `${items.length} links copied` : 'Link copied');
  } catch {
    toast('Could not copy to the clipboard', true);
  }
}

let toastTimer;
function toast(message, isError = false) {
  const el = $('toast');
  el.textContent = message;
  el.classList.toggle('error', isError);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), isError ? 6000 : 2500);
}

// ---------- selection ----------

function selectOnly(id) {
  state.selected = new Set([id]);
  state.anchor = id;
}

function selectRange(from, to) {
  const ids = visibleItems().map((i) => i.id);
  const a = ids.indexOf(from);
  const b = ids.indexOf(to);
  if (a < 0 || b < 0) return selectOnly(to);
  state.selected = new Set(ids.slice(Math.min(a, b), Math.max(a, b) + 1));
}

function moveSelection(step) {
  const ids = visibleItems().map((i) => i.id);
  if (!ids.length) return;
  const current = ids.indexOf(state.anchor);
  const next = ids[Math.max(0, Math.min(ids.length - 1, current < 0 ? 0 : current + step))];
  selectOnly(next);
  render();
  requestAnimationFrame(() => rows.get(next)?.scrollIntoView({ block: 'nearest' }));
}

// ---------- context menu ----------

function showMenu(x, y) {
  const selected = selectedItems();
  const one = selected.length === 1 ? selected[0] : null;
  const entries = [
    { label: 'Open', icon: 'open', enabled: one?.status === 'completed', run: () => call('open_file', { id: one.id }) },
    { label: 'Show in folder', icon: 'folder', enabled: !!one, run: () => call('show_in_folder', { id: one.id }) },
    { label: selected.length > 1 ? 'Copy links' : 'Copy link', icon: 'link', enabled: selected.length > 0, run: () => copyLinks(selected) },
    null,
    { label: 'Resume', icon: 'play', enabled: selected.some(canResume), run: resumeSelected },
    { label: 'Pause', icon: 'pause', enabled: selected.some(canPause), run: pauseSelected },
    null,
    { label: 'Remove…', icon: 'trash', enabled: selected.length > 0, run: askRemove },
  ];
  const menu = $('menu');
  menu.replaceChildren(
    ...entries.map((entry) => {
      if (!entry) return document.createElement('hr');
      const button = document.createElement('button');
      button.innerHTML = svg(entry.icon);
      button.append(entry.label);
      button.disabled = !entry.enabled;
      button.addEventListener('click', () => {
        hideMenu();
        entry.run();
      });
      return button;
    }),
  );
  menu.hidden = false;
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(x, innerWidth - r.width - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(y, innerHeight - r.height - 4))}px`;
}

const hideMenu = () => ($('menu').hidden = true);

// ---------- dialogs ----------

const LINK_RE = /\bhttps?:\/\/[^\s"'<>]+/gi;
const extractLinks = (text) => [...new Set(text.match(LINK_RE) ?? [])];

function openAdd(text = '') {
  const dialog = $('dlg-add');
  $('add-urls').value = text;
  const hint = $('add-hint');
  hint.classList.remove('error');
  hint.textContent = state.settings?.useCategories
    ? `Files go to ${state.settings.downloadDir}, sorted into folders by type.`
    : `Files go to ${state.settings?.downloadDir ?? 'your download folder'}.`;
  dialog.returnValue = '';
  dialog.showModal();
  $('add-urls').focus();
}

async function finishAdd() {
  const action = $('dlg-add').returnValue;
  if (action !== 'start' && action !== 'later') return;
  const urls = $('add-urls').value.split(/\s+/).filter(Boolean);
  const report = await call('add', { urls, start: action === 'start' });
  if (!report) return;
  if (report.added.length) {
    state.selected = new Set(report.added.map((i) => i.id));
    state.anchor = report.added[0].id;
    if (!report.added.some((i) => currentView().test(i))) state.view = 'all';
  }
  if (report.skipped.length) {
    const first = report.skipped[0];
    const more = report.skipped.length > 1 ? ` (and ${report.skipped.length - 1} more)` : '';
    toast(`Skipped ${first.url}: ${first.reason}${more}`, !report.added.length);
  }
  render();
}

let pendingRemove = [];
function askRemove() {
  const selected = selectedItems();
  if (!selected.length) return;
  pendingRemove = selected.map((i) => i.id);
  $('remove-text').textContent =
    selected.length === 1 ? `Remove “${selected[0].fileName}” from the list?` : `Remove ${selected.length} downloads from the list?`;
  $('remove-files').checked = false;
  $('dlg-remove').returnValue = '';
  $('dlg-remove').showModal();
}

function openSettings() {
  const s = state.settings;
  if (!s) return;
  $('set-dir').value = s.downloadDir;
  $('set-categories').checked = s.useCategories;
  $('set-conns').value = s.connections;
  $('set-conns-out').value = s.connections;
  $('set-active').value = s.maxActive;
  $('set-limit-on').checked = s.speedLimit > 0;
  const useMb = s.speedLimit === 0 || s.speedLimit >= 1024 * 1024;
  const unit = useMb ? 1024 * 1024 : 1024;
  $('set-limit-unit').value = String(unit);
  $('set-limit').value = s.speedLimit > 0 ? +(s.speedLimit / unit).toFixed(2) : 2;
  syncLimitFields();
  $('set-resume').checked = s.resumeOnStartup;
  $('set-tray').checked = s.closeToTray;
  $('set-notify').checked = s.notifyOnComplete;
  $('dlg-settings').returnValue = '';
  $('dlg-settings').showModal();
}

function syncLimitFields() {
  const on = $('set-limit-on').checked;
  $('set-limit').disabled = !on;
  $('set-limit-unit').disabled = !on;
}

async function saveSettings() {
  if ($('dlg-settings').returnValue !== 'save') return;
  const limit = parseFloat($('set-limit').value);
  const settings = {
    ...state.settings,
    downloadDir: $('set-dir').value.trim() || state.settings.downloadDir,
    useCategories: $('set-categories').checked,
    connections: Number($('set-conns').value),
    maxActive: Math.max(1, Math.min(10, Math.round(Number($('set-active').value) || 1))),
    speedLimit: $('set-limit-on').checked && limit > 0 ? Math.round(limit * Number($('set-limit-unit').value)) : 0,
    resumeOnStartup: $('set-resume').checked,
    closeToTray: $('set-tray').checked,
    notifyOnComplete: $('set-notify').checked,
  };
  const saved = await call('set_settings', { settings });
  if (saved) {
    state.settings = saved;
    toast('Settings saved');
    render();
  }
}

// ---------- events from the app ----------

function onEvent(event) {
  switch (event.type) {
    case 'updated':
      state.items.set(event.item.id, event.item);
      if (event.item.status !== 'downloading') state.live.delete(event.item.id);
      break;
    case 'removed':
      state.items.delete(event.id);
      state.live.delete(event.id);
      state.selected.delete(event.id);
      break;
    case 'progress':
      state.live = new Map(event.items.map((live) => [live.id, live]));
      state.totalSpeed = event.totalSpeed;
      for (const live of event.items) {
        const item = state.items.get(live.id);
        if (!item) continue;
        item.downloaded = live.downloaded;
        if (live.total != null) item.total = live.total;
      }
      break;
    default:
      return;
  }
  render();
}

// ---------- wiring ----------

function wire() {
  $('btn-add').addEventListener('click', () => openAdd());
  $('btn-resume').addEventListener('click', resumeSelected);
  $('btn-pause').addEventListener('click', pauseSelected);
  $('btn-remove').addEventListener('click', askRemove);
  $('btn-resume-all').addEventListener('click', () => call('resume_all'));
  $('btn-pause-all').addEventListener('click', () => call('pause_all'));
  $('btn-folder').addEventListener('click', () => call('open_download_folder'));
  $('btn-settings').addEventListener('click', openSettings);
  $('sb-limit').addEventListener('click', openSettings);
  $('search').addEventListener('input', (e) => {
    state.search = e.target.value;
    render();
  });

  for (const th of document.querySelectorAll('th[data-sort]')) {
    th.addEventListener('click', () => {
      const key = th.dataset.sort;
      state.sort = state.sort.key === key ? { key, dir: -state.sort.dir } : { key, dir: key === 'name' || key === 'status' ? 1 : -1 };
      render();
    });
  }

  const tbody = $('rows');
  tbody.addEventListener('mousedown', (e) => {
    const tr = e.target.closest('tr');
    if (!tr) return;
    const id = Number(tr.dataset.id);
    if (e.button === 2) {
      if (!state.selected.has(id)) selectOnly(id);
    } else if (e.shiftKey && state.anchor != null) {
      selectRange(state.anchor, id);
    } else if (e.ctrlKey || e.metaKey) {
      if (state.selected.has(id)) state.selected.delete(id);
      else state.selected.add(id);
      state.anchor = id;
    } else {
      selectOnly(id);
    }
    render();
  });
  tbody.addEventListener('dblclick', (e) => {
    const item = state.items.get(Number(e.target.closest('tr')?.dataset.id));
    if (item) defaultAction(item);
  });
  tbody.addEventListener('contextmenu', (e) => {
    const tr = e.target.closest('tr');
    if (!tr) return;
    e.preventDefault();
    renderNow();
    showMenu(e.clientX, e.clientY);
  });
  $('table-wrap').addEventListener('mousedown', (e) => {
    if (!e.target.closest('tr, th') && state.selected.size) {
      state.selected.clear();
      render();
    }
  });

  // The webview's own menu (Reload, Inspect…) is not useful here.
  document.addEventListener('contextmenu', (e) => {
    if (!e.target.closest('input, textarea')) e.preventDefault();
  });
  document.addEventListener('mousedown', (e) => {
    if (!e.target.closest('#menu')) hideMenu();
  });
  window.addEventListener('blur', hideMenu);
  window.addEventListener('resize', () => {
    hideMenu();
    render();
  });
  $('table-wrap').addEventListener('scroll', hideMenu);

  $('dlg-add').addEventListener('close', finishAdd);
  $('dlg-add').querySelector('form').addEventListener('submit', (e) => {
    const action = e.submitter?.value;
    if (action !== 'start' && action !== 'later') return;
    if (!$('add-urls').value.trim()) {
      e.preventDefault();
      $('add-hint').textContent = 'Paste at least one link.';
      $('add-hint').classList.add('error');
    }
  });
  $('add-urls').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      $('add-start').click();
    }
  });
  $('dlg-remove').addEventListener('close', () => {
    if ($('dlg-remove').returnValue === 'remove') {
      call('remove', { ids: pendingRemove, deleteFiles: $('remove-files').checked });
    }
  });
  $('dlg-settings').addEventListener('close', saveSettings);
  $('set-conns').addEventListener('input', (e) => ($('set-conns-out').value = e.target.value));
  $('set-limit-on').addEventListener('change', syncLimitFields);
  $('set-browse').addEventListener('click', async () => {
    const dir = await call('pick_folder');
    if (dir) $('set-dir').value = dir;
  });

  document.addEventListener('keydown', onKey);
  document.addEventListener('paste', (e) => {
    if (e.target.closest?.('input, textarea') || document.querySelector('dialog[open]')) return;
    const links = extractLinks(e.clipboardData?.getData('text') ?? '');
    if (links.length) {
      e.preventDefault();
      openAdd(links.join('\n'));
    }
  });

  // Dropping links (from a browser's address bar or a page) onto the window.
  let dragDepth = 0;
  const carriesText = (e) => [...(e.dataTransfer?.types ?? [])].some((t) => t === 'text/uri-list' || t === 'text/plain');
  window.addEventListener('dragenter', (e) => {
    if (!carriesText(e)) return;
    e.preventDefault();
    dragDepth++;
    $('drop').hidden = false;
  });
  window.addEventListener('dragover', (e) => {
    if (!carriesText(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  });
  window.addEventListener('dragleave', () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) $('drop').hidden = true;
  });
  window.addEventListener('drop', (e) => {
    e.preventDefault();
    dragDepth = 0;
    $('drop').hidden = true;
    const text = e.dataTransfer?.getData('text/uri-list') || e.dataTransfer?.getData('text/plain') || '';
    const links = extractLinks(text);
    if (links.length) openAdd(links.join('\n'));
    else if (carriesText(e)) toast('No links found in what was dropped', true);
  });
}

function onKey(e) {
  const mod = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  if (document.querySelector('dialog[open]')) return;
  const inField = e.target.closest?.('input, textarea, select');
  if (mod && key === 'n') {
    e.preventDefault();
    openAdd();
  } else if (mod && key === 'f') {
    e.preventDefault();
    $('search').focus();
  } else if (key === 'escape') {
    if (!$('menu').hidden) hideMenu();
    else if (inField) e.target.blur();
    else {
      state.selected.clear();
      render();
    }
  } else if (inField) {
    // Typing in the search box.
  } else if (mod && key === 'a') {
    e.preventDefault();
    state.selected = new Set(visibleItems().map((i) => i.id));
    render();
  } else if (key === 'delete' || (key === 'backspace' && e.metaKey)) {
    askRemove();
  } else if (key === 'enter') {
    const selected = selectedItems();
    if (selected.length === 1) defaultAction(selected[0]);
  } else if (key === ' ') {
    e.preventDefault();
    if (selectedItems().some(canPause)) pauseSelected();
    else resumeSelected();
  } else if (key === 'arrowdown' || key === 'arrowup') {
    e.preventDefault();
    moveSelection(key === 'arrowdown' ? 1 : -1);
  }
}

async function init() {
  for (const el of document.querySelectorAll('[data-icon]')) el.insertAdjacentHTML('afterbegin', svg(el.dataset.icon));
  if (/Mac/.test(navigator.platform)) {
    for (const kbd of document.querySelectorAll('kbd')) if (kbd.textContent === 'Ctrl') kbd.textContent = '⌘';
    for (const el of document.querySelectorAll('[title*="Ctrl+"]')) el.title = el.title.replace('Ctrl+', '⌘');
  }
  if (!tauri) {
    setText($('empty-title'), 'Open this page from the Slipstream app');
    $('empty').hidden = false;
    return;
  }
  wire();
  // Listen first, so nothing that changes while the list loads is missed.
  await tauri.event.listen('slipstream', (e) => onEvent(e.payload));
  const [items, settings] = await Promise.all([invoke('list'), invoke('get_settings')]);
  state.settings = settings;
  for (const item of items) if (!state.items.has(item.id)) state.items.set(item.id, item);
  renderNow();
}

init();
