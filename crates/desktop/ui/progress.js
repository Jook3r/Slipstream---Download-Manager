// A download's own window, after IDM's: status, a progress bar, the
// connections at work, a per-download speed limiter, and what to do when it
// finishes. When the download completes it becomes the "Download complete"
// window.

import {
  $,
  call,
  closeOnEscape,
  decorateIcons,
  fmtBytes,
  fmtEta,
  fmtExactBytes,
  fmtPercent,
  fmtSpeed,
  friendlyError,
  invoke,
  onEvents,
  params,
  percent,
  resizeWindow,
  setText,
  thisWindow,
} from './common.js';

const id = Number(params.get('id'));
const WIDTH = 560;
let item = null;
let live = null;
let settings = null;
let showDetails = true;
let lastTitle = '';
let lastHeight = 0;

const canStop = (i) => i.status === 'downloading' || i.status === 'queued';
const canResume = (i) => i.status === 'paused' || i.status === 'failed';

try {
  showDetails = localStorage.getItem('slipstream.details') !== 'hidden';
} catch {
  // Default to showing details.
}

function render() {
  if (!item) return;
  const complete = item.status === 'completed';
  $('progress-view').hidden = complete;
  $('complete-view').hidden = !complete;
  if (complete) renderComplete();
  else renderProgress();
  requestAnimationFrame(fitWindow);
}

/** Sizes the window to what it shows, like IDM's dialog growing with details. */
function fitWindow() {
  const section = $('complete-view').hidden ? $('progress-view') : $('complete-view');
  const height = Math.ceil(section.getBoundingClientRect().height) + 24;
  if (Math.abs(height - lastHeight) > 2) {
    lastHeight = height;
    resizeWindow(WIDTH, height);
  }
}

function setTitle(title) {
  if (title !== lastTitle) {
    lastTitle = title;
    document.title = title;
    thisWindow().setTitle(title).catch(() => {});
  }
}

function statusText() {
  switch (item.status) {
    case 'downloading':
      if (!live) return 'Connecting…';
      return live.connections > 0 ? 'Receiving data…' : 'Connecting…';
    case 'queued':
      return 'Waiting in the queue…';
    case 'paused':
      return 'Stopped';
    case 'failed':
      return friendlyError(item.error ?? 'the download failed');
    default:
      return '';
  }
}

function renderProgress() {
  const pct = percent(item);
  setTitle(pct != null ? `${Math.floor(pct)}% ${item.fileName}` : item.fileName);

  setText($('url'), item.url);
  $('url').title = item.url;
  const status = $('status');
  setText(status, statusText());
  status.title = item.status === 'failed' ? item.error ?? '' : '';
  status.classList.toggle('error', item.status === 'failed');
  setText($('size'), item.total != null ? fmtBytes(item.total) : 'Unknown');
  setText($('downloaded'), pct != null ? `${fmtBytes(item.downloaded)} (${fmtPercent(pct)})` : fmtBytes(item.downloaded));
  const speed = item.status === 'downloading' ? live?.speed ?? 0 : 0;
  setText($('rate'), fmtSpeed(speed, '/sec'));
  setText($('eta'), speed > 0 && item.total ? fmtEta((item.total - item.downloaded) / speed) : '');
  setText($('resume'), item.resumable == null ? 'Unknown' : item.resumable ? 'Yes' : 'No');

  const bar = $('bar');
  bar.querySelector('i').style.width = `${pct ?? 0}%`;
  bar.classList.toggle('paused', item.status === 'paused' || item.status === 'queued');
  bar.classList.toggle('failed', item.status === 'failed');

  const pause = $('pause');
  setText(pause, canResume(item) ? 'Resume' : 'Pause');
  pause.disabled = !canResume(item) && !canStop(item);
  setText($('details-toggle'), showDetails ? '« Hide details' : 'Show details »');
  $('details').hidden = !showDetails;
  if (showDetails) renderDetails();
}

function renderDetails() {
  drawSegments();
  const tbody = $('conns');
  const segments = item.status === 'downloading' ? live?.segments ?? [] : [];
  if (!segments.length) {
    const message =
      item.status === 'downloading' ? 'Connecting…' : 'Connections appear here while the file downloads.';
    if (tbody.dataset.empty !== message) {
      tbody.dataset.empty = message;
      tbody.innerHTML = '<tr><td class="none" colspan="3"></td></tr>';
      tbody.querySelector('td').textContent = message;
    }
    return;
  }
  if (tbody.dataset.empty) {
    delete tbody.dataset.empty;
    tbody.replaceChildren();
  }
  while (tbody.children.length > segments.length) tbody.lastChild.remove();
  while (tbody.children.length < segments.length) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td class="n"></td><td class="got"></td><td class="info"></td>';
    tbody.append(tr);
  }
  segments.forEach((s, k) => {
    const tr = tbody.children[k];
    const done = s.written >= s.end;
    tr.className = s.active && !done ? 'active' : '';
    setText(tr.querySelector('.n'), String(k + 1));
    setText(tr.querySelector('.got'), fmtBytes(s.written - s.start));
    setText(tr.querySelector('.info'), done ? 'Complete' : s.active ? 'Receiving data…' : 'Waiting');
  });
}

/** IDM's bar: received data per piece, a marker where each piece starts. */
function drawSegments() {
  const canvas = $('segmap');
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
  const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const ctx = canvas.getContext('2d');
  const css = getComputedStyle(document.documentElement);
  const color = (name) => css.getPropertyValue(name).trim();
  ctx.fillStyle = color('--track');
  ctx.fillRect(0, 0, w, h);
  const total = live?.total ?? item.total;
  if (!total) return;
  const x = (bytes) => (bytes / total) * w;
  ctx.fillStyle = color('--accent');
  const segments = item.status === 'downloading' ? live?.segments : null;
  if (segments?.length) {
    for (const s of segments) ctx.fillRect(x(s.start), 0, Math.max(0, x(s.written) - x(s.start)), h);
    ctx.fillStyle = color('--marker');
    for (const s of segments) if (s.start > 0) ctx.fillRect(Math.round(x(s.start)), 0, Math.max(1, dpr), h);
  } else {
    ctx.fillRect(0, 0, x(item.downloaded), h);
  }
}

function renderComplete() {
  setTitle('Download complete');
  const size = item.total ?? item.downloaded;
  setText($('done-size'), `Downloaded ${fmtBytes(size)} (${fmtExactBytes(size)})`);
  $('done-url').value = item.url;
  $('done-path').value = item.path ?? '';
  $('dont-show').checked = settings ? !settings.showComplete : false;
}

function showTab(tabId) {
  for (const b of document.querySelectorAll('.tabs button')) b.classList.toggle('active', b.dataset.tab === tabId);
  for (const tab of ['tab-status', 'tab-limiter', 'tab-done']) $(tab).hidden = tab !== tabId;
}

async function setShowComplete(show) {
  if (!settings) return;
  const saved = await call('set_settings', { settings: { ...settings, showComplete: show } });
  if (saved) settings = saved;
}

function applyLimit() {
  const kb = parseFloat($('limit').value);
  const on = $('limit-on').checked && kb > 0;
  $('limit').disabled = !$('limit-on').checked;
  call('set_speed_limit', { id, bytesPerSec: on ? Math.round(kb * 1024) : 0 });
}

function wire() {
  for (const b of document.querySelectorAll('.tabs button')) b.addEventListener('click', () => showTab(b.dataset.tab));
  $('details-toggle').addEventListener('click', () => {
    showDetails = !showDetails;
    try {
      localStorage.setItem('slipstream.details', showDetails ? 'shown' : 'hidden');
    } catch {
      // Not remembered; fine.
    }
    render();
  });
  $('pause').addEventListener('click', () => {
    if (canResume(item)) call('resume', { ids: [id] });
    else if (canStop(item)) call('pause', { ids: [id] });
  });
  $('cancel').addEventListener('click', async () => {
    if (canStop(item)) await call('pause', { ids: [id] });
    thisWindow().close();
  });
  $('limit-on').addEventListener('change', applyLimit);
  $('limit').addEventListener('change', applyLimit);
  $('show-complete').addEventListener('change', (e) => setShowComplete(e.target.checked));
  $('open-when-done').addEventListener('change', (e) => call('set_open_when_done', { id, open: e.target.checked }));

  $('open').addEventListener('click', async () => {
    await call('open_file', { id });
    thisWindow().close();
  });
  $('open-folder').addEventListener('click', () => call('show_in_folder', { id }));
  $('close').addEventListener('click', () => thisWindow().close());
  $('dont-show').addEventListener('change', (e) => setShowComplete(!e.target.checked));
  closeOnEscape();
}

async function init() {
  decorateIcons();
  wire();
  await onEvents((event) => {
    if (event.type === 'updated' && event.item.id === id) {
      item = event.item;
      if (item.status !== 'downloading') live = null;
    } else if (event.type === 'removed' && event.id === id) {
      thisWindow().close();
      return;
    } else if (event.type === 'progress') {
      live = event.items.find((l) => l.id === id) ?? null;
      if (live && item) {
        item.downloaded = live.downloaded;
        if (live.total != null) item.total = live.total;
      }
    } else {
      return;
    }
    render();
  });
  [item, settings] = await Promise.all([invoke('get_item', { id }), invoke('get_settings')]);
  if (!item) {
    thisWindow().close();
    return;
  }
  $('limit-on').checked = item.speedLimit > 0;
  $('limit').value = item.speedLimit > 0 ? Math.round(item.speedLimit / 1024) : 512;
  $('limit').disabled = !$('limit-on').checked;
  $('show-complete').checked = settings.showComplete;
  $('open-when-done').checked = item.openWhenDone;
  render();
}

init();
