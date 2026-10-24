// Slipstream's main window: the download list with IDM's layout. Each
// download's details live in its own window (progress.html), opened by
// double-clicking it.

import {
  $,
  CATEGORIES,
  call,
  decorateIcons,
  fmtBytes,
  fmtDate,
  fmtEta,
  fmtPercent,
  fmtSpeed,
  invoke,
  onEvents,
  percent,
  setText,
  svg,
  tauri,
  toast,
} from './common.js';

// ---------- the category tree ----------

const TREE = [
  {
    id: 'all',
    label: 'All Downloads',
    icon: 'all',
    tint: 'node-folder',
    test: () => true,
    children: CATEGORIES.map((c) => ({ id: `cat:${c.id}`, label: c.label, icon: c.id, tint: `cat-${c.id}`, test: (i) => i.category === c.id })),
  },
  {
    id: 'unfinished',
    label: 'Unfinished',
    icon: 'unfinished',
    tint: 'node-unfinished',
    test: (i) => i.status !== 'completed',
    children: [
      { id: 'downloading', label: 'Downloading', icon: 'downloading', tint: 'node-unfinished', test: (i) => i.status === 'downloading' },
      { id: 'queued', label: 'Queued', icon: 'queued', tint: 'node-unfinished', test: (i) => i.status === 'queued' },
    ],
  },
  { id: 'finished', label: 'Finished', icon: 'finished', tint: 'node-finished', test: (i) => i.status === 'completed' },
];
const VIEWS = new Map(TREE.flatMap((n) => [n, ...(n.children ?? [])]).map((n) => [n.id, n]));

const STATUS_ORDER = { downloading: 0, queued: 1, paused: 2, failed: 3, completed: 4 };
const canResume = (i) => i.status === 'paused' || i.status === 'failed';
const canStop = (i) => i.status === 'downloading' || i.status === 'queued';

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
  expanded: new Set(['all', 'unfinished']),
  showCategories: true,
};
const rows = new Map();

// Remember layout choices on this computer; nothing breaks without them.
const PREFS_KEY = 'slipstream.main';
function loadPrefs() {
  try {
    const p = JSON.parse(localStorage.getItem(PREFS_KEY) ?? '{}');
    if (p.sort) state.sort = p.sort;
    if (Array.isArray(p.expanded)) state.expanded = new Set(p.expanded);
    if (typeof p.showCategories === 'boolean') state.showCategories = p.showCategories;
  } catch {
    // Ignore unreadable preferences.
  }
}
function savePrefs() {
  try {
    localStorage.setItem(
      PREFS_KEY,
      JSON.stringify({ sort: state.sort, expanded: [...state.expanded], showCategories: state.showCategories }),
    );
  } catch {
    // Storage may be unavailable; that is fine.
  }
}

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

const allItems = () => [...state.items.values()];

/** Queue positions, oldest first, as the manager runs them. */
function queuePositions() {
  const positions = new Map();
  let n = 0;
  for (const item of allItems()) if (item.status === 'queued') positions.set(item.id, ++n);
  return positions;
}

function visibleItems(queue = queuePositions()) {
  const view = VIEWS.get(state.view) ?? VIEWS.get('all');
  const q = state.search.trim().toLowerCase();
  const list = allItems().filter(
    (i) =>
      view.test(i) &&
      (!q || i.fileName.toLowerCase().includes(q) || i.url.toLowerCase().includes(q) || i.description.toLowerCase().includes(q)),
  );
  const key = {
    name: (i) => i.fileName.toLowerCase(),
    queue: (i) => queue.get(i.id) ?? Infinity,
    size: (i) => i.total ?? -1,
    status: (i) => STATUS_ORDER[i.status],
    rate: (i) => state.live.get(i.id)?.speed ?? 0,
    tried: (i) => i.lastTryAt ?? 0,
    added: (i) => i.addedAt,
    description: (i) => i.description.toLowerCase(),
  }[state.sort.key] ?? ((i) => i.addedAt);
  list.sort((a, b) => {
    const x = key(a);
    const y = key(b);
    return (x < y ? -1 : x > y ? 1 : a.id - b.id) * state.sort.dir;
  });
  return list;
}

const selectedItems = () => [...state.selected].map((id) => state.items.get(id)).filter(Boolean);

function renderNow() {
  for (const id of state.selected) if (!state.items.has(id)) state.selected.delete(id);
  for (const id of rows.keys()) if (!state.items.has(id)) rows.delete(id);

  const queue = queuePositions();
  const list = visibleItems(queue);
  const tbody = $('rows');
  for (const item of list) updateRow(rows.get(item.id) ?? createRow(item.id), item, queue);
  const shown = [...tbody.children].map((tr) => Number(tr.dataset.id));
  if (shown.length !== list.length || list.some((item, k) => item.id !== shown[k])) {
    tbody.replaceChildren(...list.map((item) => rows.get(item.id)));
  }

  $('categories').hidden = !state.showCategories;
  renderEmpty(list.length);
  renderTree();
  renderToolbar();
  renderStatusbar();
  for (const th of document.querySelectorAll('th[data-sort]')) {
    const active = th.dataset.sort === state.sort.key;
    th.classList.toggle('sorted', active);
    th.classList.toggle('asc', active && state.sort.dir > 0);
  }
}

function createRow(id) {
  const tr = document.createElement('tr');
  tr.dataset.id = id;
  tr.innerHTML =
    '<td><div class="name-cell"><span class="cat"></span><span class="fname"></span></div></td>' +
    '<td class="num q"></td><td class="num size"></td>' +
    '<td><div class="status-cell"><span class="mini"><i></i></span><span class="stext"></span></div></td>' +
    '<td class="num eta"></td><td class="num rate"></td><td class="tried"></td><td class="added"></td><td class="desc"></td>';
  rows.set(id, tr);
  return tr;
}

function statusText(item) {
  const pct = percent(item);
  switch (item.status) {
    case 'completed':
      return 'Complete';
    case 'failed':
      return 'Error';
    case 'downloading':
      if (pct != null) return fmtPercent(pct);
      return item.downloaded > 0 ? fmtBytes(item.downloaded) : 'Connecting…';
    case 'paused':
      return pct != null ? `Paused ${fmtPercent(pct)}` : 'Paused';
    default:
      return pct ? `Queued ${fmtPercent(pct)}` : 'Queued';
  }
}

function updateRow(tr, item, queue) {
  const live = state.live.get(item.id);
  const pct = percent(item);
  tr.className = `st-${item.status}${state.selected.has(item.id) ? ' selected' : ''}${pct ? '' : ' fresh'}`;
  const cat = tr.querySelector('.cat');
  if (cat.dataset.cat !== item.category) {
    cat.dataset.cat = item.category;
    cat.className = `cat cat-${item.category}`;
    cat.innerHTML = svg(item.category);
  }
  const name = tr.querySelector('.fname');
  setText(name, item.fileName);
  tr.title = item.path ?? item.url;
  setText(tr.querySelector('.q'), String(queue.get(item.id) ?? ''));
  setText(tr.querySelector('.size'), fmtBytes(item.total));
  tr.querySelector('.mini i').style.width = `${pct ?? 0}%`;
  const stext = tr.querySelector('.stext');
  setText(stext, statusText(item));
  stext.title = item.error ?? '';
  const speed = item.status === 'downloading' && live ? live.speed : 0;
  setText(tr.querySelector('.rate'), fmtSpeed(speed));
  setText(tr.querySelector('.eta'), speed > 0 && item.total ? fmtEta((item.total - item.downloaded) / speed) : '');
  setText(tr.querySelector('.tried'), fmtDate(item.lastTryAt));
  setText(tr.querySelector('.added'), fmtDate(item.addedAt));
  setText(tr.querySelector('.desc'), item.description);
}

function renderEmpty(visibleCount) {
  $('empty').hidden = visibleCount > 0;
  if (visibleCount > 0) return;
  const view = VIEWS.get(state.view);
  if (state.items.size === 0) {
    setText($('empty-title'), 'No downloads yet');
    $('empty-hint').hidden = false;
  } else {
    setText($('empty-title'), state.search ? `Nothing matches “${state.search}”` : `Nothing in ${view.label}`);
    $('empty-hint').hidden = true;
  }
}

function renderTree() {
  const tree = $('tree');
  if (!tree.childElementCount) {
    const addNode = (node, depth) => {
      const row = document.createElement('div');
      row.className = 'node';
      row.dataset.view = node.id;
      row.setAttribute('role', 'treeitem');
      row.style.paddingLeft = `${4 + depth * 16}px`;
      row.innerHTML = `<span class="twisty${node.children ? '' : ' leaf'}">${svg('chevron')}</span><span class="cat ${node.tint}">${svg(node.icon)}</span><span class="label"></span><span class="count"></span>`;
      row.querySelector('.label').textContent = node.label;
      row.querySelector('.twisty').addEventListener('mousedown', (e) => {
        e.stopPropagation();
        if (state.expanded.has(node.id)) state.expanded.delete(node.id);
        else state.expanded.add(node.id);
        savePrefs();
        render();
      });
      row.addEventListener('mousedown', () => {
        state.view = node.id;
        render();
      });
      tree.append(row);
      for (const child of node.children ?? []) addNode({ ...child, parent: node.id }, depth + 1);
    };
    for (const node of TREE) addNode(node, 0);
  }
  const items = allItems();
  for (const row of tree.children) {
    const node = VIEWS.get(row.dataset.view);
    const parent = TREE.find((n) => n.children?.some((c) => c.id === node.id));
    row.hidden = parent ? !state.expanded.has(parent.id) : false;
    row.classList.toggle('open', state.expanded.has(node.id));
    row.classList.toggle('active', node.id === state.view);
    const count = items.filter(node.test).length;
    setText(row.querySelector('.count'), count ? String(count) : '');
  }
}

function renderToolbar() {
  const selected = selectedItems();
  const all = allItems();
  $('tb-resume').disabled = !selected.some(canResume);
  $('tb-stop').disabled = !selected.some(canStop);
  $('tb-stop-all').disabled = !all.some(canStop);
  $('tb-delete').disabled = selected.length === 0;
  $('tb-delete-done').disabled = !all.some((i) => i.status === 'completed');
  $('tb-start-queue').disabled = !all.some(canResume);
  $('tb-stop-queue').disabled = !all.some((i) => i.status === 'queued');
}

function renderStatusbar() {
  const all = allItems();
  const count = (status) => all.filter((i) => i.status === status).length;
  setText($('sb-speed'), state.totalSpeed >= 1 ? `↓ ${fmtSpeed(state.totalSpeed)}` : '');
  const parts = [
    [count('downloading'), 'downloading'],
    [count('queued'), 'in queue'],
    [count('paused'), 'stopped'],
    [count('failed'), 'with errors'],
    [count('completed'), 'complete'],
  ]
    .filter(([n]) => n > 0)
    .map(([n, label]) => `${n} ${label}`);
  setText($('sb-counts'), parts.join(' · ') || 'No downloads');
  const limit = state.settings?.speedLimit ?? 0;
  setText($('sb-limit'), limit > 0 ? `Speed limit ${fmtSpeed(limit)}` : 'No speed limit');
}

// ---------- actions ----------

const ids = (items) => items.map((i) => i.id);
const resumeSelected = () => call('resume', { ids: ids(selectedItems().filter(canResume)) });
const stopSelected = () => call('pause', { ids: ids(selectedItems().filter(canStop)) });
const stopQueue = () => call('pause', { ids: ids(allItems().filter((i) => i.status === 'queued')) });
const openWindow = (item) => call('open_progress', { id: item.id });

function openItem(item) {
  if (item.status === 'completed') call('open_file', { id: item.id });
  else openWindow(item);
}

async function deleteCompleted() {
  const n = await call('delete_completed');
  if (n) toast(`${n} finished download${n > 1 ? 's' : ''} taken off the list`);
}

async function copyLinks(items) {
  try {
    await navigator.clipboard.writeText(items.map((i) => i.url).join('\n'));
    toast(items.length > 1 ? `${items.length} links copied` : 'Link copied');
  } catch {
    toast('Could not copy to the clipboard', true);
  }
}

const LINK_RE = /\bhttps?:\/\/[^\s"'<>]+/gi;
const extractLinks = (text) => [...new Set(text.match(LINK_RE) ?? [])];

function openAdd(text = '') {
  $('add-urls').value = text;
  $('add-hint').textContent = '';
  $('add-hint').classList.remove('error');
  $('dlg-add').returnValue = '';
  $('dlg-add').showModal();
  $('add-urls').focus();
  $('add-urls').select();
}

let pendingDelete = [];
function askDelete() {
  const selected = selectedItems();
  if (!selected.length) return;
  pendingDelete = ids(selected);
  $('delete-text').textContent =
    selected.length === 1 ? `Delete “${selected[0].fileName}” from the list?` : `Delete ${selected.length} downloads from the list?`;
  $('delete-files').checked = false;
  $('dlg-delete').returnValue = '';
  $('dlg-delete').showModal();
}

// ---------- menus ----------

/** Drop-down menus of the menu bar. Entries: { label, key, enabled, checked, run } or '-'. */
function menuEntries(name) {
  const selected = selectedItems();
  const one = selected.length === 1 ? selected[0] : null;
  const all = allItems();
  return {
    tasks: [
      { label: 'Add new download…', key: 'Ctrl+N', run: () => openAdd() },
      { label: 'Open download folder', run: () => call('open_download_folder') },
      '-',
      { label: 'Exit', run: () => call('quit_app') },
    ],
    file: [
      { label: 'Open', key: 'Enter', enabled: one?.status === 'completed', run: () => call('open_file', { id: one.id }) },
      { label: 'Open folder', enabled: !!one, run: () => call('show_in_folder', { id: one.id }) },
      { label: 'Properties', enabled: !!one, run: () => openWindow(one) },
      '-',
      { label: 'Delete…', key: 'Del', enabled: selected.length > 0, run: askDelete },
      { label: 'Delete completed', enabled: all.some((i) => i.status === 'completed'), run: deleteCompleted },
    ],
    downloads: [
      { label: 'Resume', enabled: selected.some(canResume), run: resumeSelected },
      { label: 'Stop', enabled: selected.some(canStop), run: stopSelected },
      { label: 'Stop all', enabled: all.some(canStop), run: () => call('pause_all') },
      '-',
      { label: 'Start queue', enabled: all.some(canResume), run: () => call('resume_all') },
      { label: 'Stop queue', enabled: all.some((i) => i.status === 'queued'), run: stopQueue },
      '-',
      { label: 'Options…', run: openOptions },
    ],
    view: [
      {
        label: 'Categories',
        checked: state.showCategories,
        run: () => {
          state.showCategories = !state.showCategories;
          savePrefs();
          render();
        },
      },
    ],
    help: [
      { label: 'Browser integration…', run: openIntegration },
      { label: 'About Slipstream', run: () => $('dlg-about').showModal() },
    ],
    row: [
      { label: 'Open', icon: 'open', enabled: one?.status === 'completed', run: () => call('open_file', { id: one.id }) },
      { label: 'Open folder', icon: 'folder', enabled: !!one, run: () => call('show_in_folder', { id: one.id }) },
      '-',
      { label: 'Resume download', icon: 'play', enabled: selected.some(canResume), run: resumeSelected },
      { label: 'Stop download', icon: 'pause', enabled: selected.some(canStop), run: stopSelected },
      '-',
      { label: selected.length > 1 ? 'Copy links' : 'Copy link', icon: 'link', enabled: selected.length > 0, run: () => copyLinks(selected) },
      { label: 'Delete…', icon: 'trash', enabled: selected.length > 0, run: askDelete },
      '-',
      { label: 'Properties', icon: 'info', enabled: !!one, run: () => openWindow(one) },
    ],
  }[name];
}

let openMenuName = null;
function showMenu(name, x, y) {
  const menu = $('menu');
  menu.replaceChildren(
    ...menuEntries(name).map((entry) => {
      if (entry === '-') return document.createElement('hr');
      const button = document.createElement('button');
      const mark = entry.checked ? svg('check') : entry.icon ? svg(entry.icon) : '';
      button.innerHTML = `<span class="mark">${mark}</span><span class="mlabel"></span><span class="key"></span>`;
      button.querySelector('.mlabel').textContent = entry.label;
      button.querySelector('.key').textContent = entry.key?.replace('Ctrl+', /Mac/.test(navigator.platform) ? '⌘' : 'Ctrl+') ?? '';
      button.disabled = entry.enabled === false;
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
  openMenuName = name;
  for (const b of document.querySelectorAll('.menubar button')) b.classList.toggle('open', b.dataset.menu === name);
}

function hideMenu() {
  $('menu').hidden = true;
  openMenuName = null;
  for (const b of document.querySelectorAll('.menubar button')) b.classList.remove('open');
}

function showBarMenu(button) {
  const r = button.getBoundingClientRect();
  showMenu(button.dataset.menu, r.left, r.bottom + 2);
}

// ---------- dialogs ----------

let rememberedDirs = {};
function openOptions() {
  const s = state.settings;
  if (!s) return;
  $('set-fileinfo').checked = s.showFileInfo;
  $('set-progress').checked = s.showProgress;
  $('set-complete').checked = s.showComplete;
  $('set-notify').checked = s.notifyOnComplete;
  $('set-tray').checked = s.closeToTray;
  $('set-resume').checked = s.resumeOnStartup;
  $('set-dir').value = s.downloadDir;
  $('set-categories').checked = s.useCategories;
  rememberedDirs = { ...s.categoryDirs };
  renderRemembered();
  $('set-conns').value = s.connections;
  $('set-conns-out').value = s.connections;
  $('set-active').value = s.maxActive;
  $('set-limit-on').checked = s.speedLimit > 0;
  const unit = s.speedLimit === 0 || s.speedLimit >= 1048576 ? 1048576 : 1024;
  $('set-limit-unit').value = String(unit);
  $('set-limit').value = s.speedLimit > 0 ? +(s.speedLimit / unit).toFixed(2) : 2;
  syncLimitFields();
  showTab('opt-general');
  $('dlg-options').returnValue = '';
  $('dlg-options').showModal();
}

function renderRemembered() {
  const box = $('set-remembered');
  const entries = Object.entries(rememberedDirs);
  if (!entries.length) {
    box.innerHTML = '<p class="hint">None yet. Tick “Remember this path” in a Download File Info window.</p>';
    return;
  }
  box.replaceChildren(
    ...entries.map(([category, dir]) => {
      const row = document.createElement('div');
      row.className = 'row';
      const label = document.createElement('span');
      label.className = 'path';
      label.textContent = `${CATEGORIES.find((c) => c.id === category)?.label ?? category}: ${dir}`;
      label.title = dir;
      const forget = document.createElement('button');
      forget.type = 'button';
      forget.className = 'btn small';
      forget.textContent = 'Forget';
      forget.addEventListener('click', () => {
        delete rememberedDirs[category];
        renderRemembered();
      });
      row.append(label, forget);
      return row;
    }),
  );
}

function syncLimitFields() {
  const on = $('set-limit-on').checked;
  $('set-limit').disabled = !on;
  $('set-limit-unit').disabled = !on;
}

function showTab(id) {
  for (const b of document.querySelectorAll('#dlg-options .tabs button')) b.classList.toggle('active', b.dataset.tab === id);
  for (const page of document.querySelectorAll('#dlg-options .opt-page')) page.hidden = page.id !== id;
}

async function saveOptions() {
  if ($('dlg-options').returnValue !== 'save') return;
  const limit = parseFloat($('set-limit').value);
  const settings = {
    ...state.settings,
    showFileInfo: $('set-fileinfo').checked,
    showProgress: $('set-progress').checked,
    showComplete: $('set-complete').checked,
    notifyOnComplete: $('set-notify').checked,
    closeToTray: $('set-tray').checked,
    resumeOnStartup: $('set-resume').checked,
    downloadDir: $('set-dir').value.trim() || state.settings.downloadDir,
    useCategories: $('set-categories').checked,
    categoryDirs: rememberedDirs,
    connections: Number($('set-conns').value),
    maxActive: Math.max(1, Math.min(10, Math.round(Number($('set-active').value) || 1))),
    speedLimit: $('set-limit-on').checked && limit > 0 ? Math.round(limit * Number($('set-limit-unit').value)) : 0,
  };
  const saved = await call('set_settings', { settings });
  if (saved) {
    state.settings = saved;
    render();
  }
}

async function openIntegration() {
  const status = await call('integration_status');
  if (!status) return;
  const text = status.problem
    ? `Browser integration is not set up: ${status.problem}.`
    : status.browsers.length
      ? `Slipstream is registered with ${status.browsers.join(', ')}. Install the extension in each browser you use:`
      : 'No supported browser was found for this user. Install Chrome, Edge, Brave, Vivaldi or Firefox, then restart Slipstream.';
  $('integration-status').textContent = text;
  $('integration-ids').textContent = `Extension IDs: ${status.chromeExtensionId} (Chrome family), ${status.firefoxExtensionId} (Firefox).`;
  $('dlg-integration').showModal();
}

// ---------- events from the app ----------

function onEvent(event) {
  switch (event.type) {
    case 'updated': {
      const isNew = !state.items.has(event.item.id);
      state.items.set(event.item.id, event.item);
      if (event.item.status !== 'downloading') state.live.delete(event.item.id);
      if (isNew && !VIEWS.get(state.view).test(event.item)) state.view = 'all';
      break;
    }
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

function selectOnly(id) {
  state.selected = new Set([id]);
  state.anchor = id;
}

function selectRange(from, to) {
  const list = visibleItems().map((i) => i.id);
  const a = list.indexOf(from);
  const b = list.indexOf(to);
  if (a < 0 || b < 0) return selectOnly(to);
  state.selected = new Set(list.slice(Math.min(a, b), Math.max(a, b) + 1));
}

function moveSelection(step) {
  const list = visibleItems().map((i) => i.id);
  if (!list.length) return;
  const current = list.indexOf(state.anchor);
  const next = list[Math.max(0, Math.min(list.length - 1, current < 0 ? 0 : current + step))];
  selectOnly(next);
  render();
  requestAnimationFrame(() => rows.get(next)?.scrollIntoView({ block: 'nearest' }));
}

function wire() {
  $('tb-add').addEventListener('click', () => openAdd());
  $('tb-resume').addEventListener('click', resumeSelected);
  $('tb-stop').addEventListener('click', stopSelected);
  $('tb-stop-all').addEventListener('click', () => call('pause_all'));
  $('tb-delete').addEventListener('click', askDelete);
  $('tb-delete-done').addEventListener('click', deleteCompleted);
  $('tb-options').addEventListener('click', openOptions);
  $('tb-start-queue').addEventListener('click', () => call('resume_all'));
  $('tb-stop-queue').addEventListener('click', stopQueue);
  $('sb-limit').addEventListener('click', () => {
    openOptions();
    showTab('opt-connection');
  });
  $('cat-close').addEventListener('click', () => {
    state.showCategories = false;
    savePrefs();
    render();
  });
  $('search').addEventListener('input', (e) => {
    state.search = e.target.value;
    render();
  });

  for (const button of document.querySelectorAll('.menubar button')) {
    button.addEventListener('mousedown', (e) => {
      e.preventDefault();
      if (openMenuName === button.dataset.menu) hideMenu();
      else showBarMenu(button);
    });
    button.addEventListener('mouseenter', () => {
      if (openMenuName && openMenuName !== 'row' && openMenuName !== button.dataset.menu) showBarMenu(button);
    });
  }

  for (const th of document.querySelectorAll('th[data-sort]')) {
    th.addEventListener('click', () => {
      const key = th.dataset.sort;
      const ascending = ['name', 'queue', 'status', 'description'].includes(key);
      state.sort = state.sort.key === key ? { key, dir: -state.sort.dir } : { key, dir: ascending ? 1 : -1 };
      savePrefs();
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
    if (item) openWindow(item);
  });
  tbody.addEventListener('contextmenu', (e) => {
    if (!e.target.closest('tr')) return;
    e.preventDefault();
    renderNow();
    showMenu('row', e.clientX, e.clientY);
  });
  $('list-wrap').addEventListener('mousedown', (e) => {
    if (!e.target.closest('tr, th') && state.selected.size) {
      state.selected.clear();
      render();
    }
  });

  document.addEventListener('contextmenu', (e) => {
    if (!e.target.closest('input, textarea')) e.preventDefault();
  });
  document.addEventListener('mousedown', (e) => {
    if (!e.target.closest('#menu, .menubar')) hideMenu();
  });
  window.addEventListener('blur', hideMenu);
  window.addEventListener('resize', hideMenu);
  $('list-wrap').addEventListener('scroll', hideMenu);

  $('dlg-add').querySelector('form').addEventListener('submit', (e) => {
    if (e.submitter?.value !== 'ok') return;
    if (!extractLinks($('add-urls').value).length) {
      e.preventDefault();
      $('add-hint').textContent = 'Enter a link that starts with http:// or https://';
      $('add-hint').classList.add('error');
    }
  });
  $('dlg-add').addEventListener('close', () => {
    if ($('dlg-add').returnValue === 'ok') call('add_links', { urls: extractLinks($('add-urls').value) });
  });
  $('add-urls').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      $('add-ok').click();
    }
  });
  $('dlg-delete').addEventListener('close', () => {
    if ($('dlg-delete').returnValue === 'delete') {
      call('remove', { ids: pendingDelete, deleteFiles: $('delete-files').checked });
    }
  });
  $('dlg-options').addEventListener('close', saveOptions);
  for (const b of document.querySelectorAll('#dlg-options .tabs button')) b.addEventListener('click', () => showTab(b.dataset.tab));
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
      call('add_links', { urls: links });
    }
  });

  // Links dragged in from a browser.
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
    const links = extractLinks(e.dataTransfer?.getData('text/uri-list') || e.dataTransfer?.getData('text/plain') || '');
    if (links.length) call('add_links', { urls: links });
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
    askDelete();
  } else if (key === 'enter') {
    const selected = selectedItems();
    if (selected.length === 1) openItem(selected[0]);
  } else if (key === ' ') {
    e.preventDefault();
    if (selectedItems().some(canStop)) stopSelected();
    else resumeSelected();
  } else if (key === 'arrowdown' || key === 'arrowup') {
    e.preventDefault();
    moveSelection(key === 'arrowdown' ? 1 : -1);
  }
}

async function init() {
  decorateIcons();
  loadPrefs();
  if (!tauri) {
    setText($('empty-title'), 'Open this page from the Slipstream app');
    $('empty').hidden = false;
    return;
  }
  wire();
  // Listen first, so nothing that changes while the list loads is missed.
  await onEvents(onEvent);
  const [items, settings] = await Promise.all([invoke('list'), invoke('get_settings')]);
  state.settings = settings;
  for (const item of items) if (!state.items.has(item.id)) state.items.set(item.id, item);
  renderNow();
}

init();
