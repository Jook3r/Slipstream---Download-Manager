// IDM's Scheduler for the main download queue: when it starts and stops,
// what happens when it is done, and the files in it.

import {
  $,
  STATUS_LABEL,
  call,
  closeOnEscape,
  decorateIcons,
  fmtBytes,
  invoke,
  onEvents,
  percent,
  fmtPercent,
  thisWindow,
} from './common.js';

const DAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
let settings = null;
let queue = [];
let selected = null;
let dirty = false;

const today = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
};

function showTab(id) {
  for (const b of document.querySelectorAll('.tabs button')) b.classList.toggle('active', b.dataset.tab === id);
  for (const page of document.querySelectorAll('.sched-page')) page.hidden = page.id !== id;
}

function fillForm(s) {
  $('start').checked = s.start;
  $('start-at').value = s.startAt;
  const daily = s.days.some(Boolean);
  $('daily').checked = daily;
  $('once').checked = !daily;
  $('date').value = s.date || today();
  document.querySelectorAll('#days input').forEach((box, i) => (box.checked = s.days[i]));
  $('stop').checked = s.stop;
  $('stop-at').value = s.stopAt;
  $('when-done').value = s.whenDone;
  syncForm();
}

function readForm() {
  const daily = $('daily').checked;
  return {
    start: $('start').checked,
    startAt: $('start-at').value || '02:00',
    days: [...document.querySelectorAll('#days input')].map((box) => daily && box.checked),
    date: $('date').value || today(),
    stop: $('stop').checked,
    stopAt: $('stop-at').value || '07:00',
    whenDone: $('when-done').value,
  };
}

function syncForm() {
  const on = $('start').checked;
  $('start-at').disabled = !on;
  $('when').classList.toggle('off', !on);
  for (const el of document.querySelectorAll('#when input')) el.disabled = !on;
  $('date').disabled = !on || !$('once').checked;
  for (const box of document.querySelectorAll('#days input')) box.disabled = !on || !$('daily').checked;
  $('stop-at').disabled = !$('stop').checked;
  describe();
}

/** One line saying what will happen, so the settings are easy to check. */
function describe() {
  const s = readForm();
  const parts = [];
  if (s.start) {
    const days = s.days.some(Boolean)
      ? s.days.every(Boolean)
        ? 'every day'
        : `on ${DAYS.filter((_, i) => s.days[i]).join(', ')}`
      : `on ${new Date(`${s.date}T00:00`).toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })}`;
    parts.push(`Starts at ${s.startAt} ${days}`);
  } else {
    parts.push('Starts when you click Start Queue');
  }
  if (s.stop) parts.push(`stops at ${s.stopAt}`);
  const done = { exit: 'then Slipstream exits', sleep: 'then the computer sleeps', shutdown: 'then the computer turns off' }[s.whenDone];
  if (done) parts.push(done);
  const count = queue.length;
  parts.push(`${count} file${count === 1 ? '' : 's'} in the queue`);
  $('summary').textContent = `${parts.join(', ')}.`;
}

function statusOf(item) {
  const pct = percent(item);
  const label = STATUS_LABEL[item.status] ?? item.status;
  return pct != null && item.status !== 'completed' ? `${label} ${fmtPercent(pct)}` : label;
}

function renderQueue() {
  const tbody = $('rows');
  if (!queue.length) {
    tbody.innerHTML = '<tr><td colspan="4" class="none">The queue is empty.</td></tr>';
  } else {
    tbody.replaceChildren(
      ...queue.map((item, n) => {
        const tr = document.createElement('tr');
        tr.className = item.id === selected ? 'selected' : '';
        for (const text of [String(n + 1), item.fileName, item.total != null ? fmtBytes(item.total) : '', statusOf(item)]) {
          const td = document.createElement('td');
          td.textContent = text;
          tr.append(td);
        }
        tr.title = item.url;
        tr.addEventListener('click', () => {
          selected = item.id;
          renderQueue();
        });
        return tr;
      }),
    );
  }
  const at = queue.findIndex((i) => i.id === selected);
  $('up').disabled = at <= 0;
  $('down').disabled = at < 0 || at === queue.length - 1;
  $('remove').disabled = at < 0;
  const all = queue;
  $('start-now').disabled = !all.some((i) => i.status === 'paused' || i.status === 'failed');
  $('stop-now').disabled = !all.some((i) => i.status === 'queued' || i.status === 'downloading');
  describe();
}

async function loadQueue() {
  queue = await invoke('get_queue');
  if (!queue.some((i) => i.id === selected)) selected = null;
  renderQueue();
}

function move(step) {
  const at = queue.findIndex((i) => i.id === selected);
  const to = at + step;
  if (at < 0 || to < 0 || to >= queue.length) return;
  const order = queue.map((i) => i.id);
  [order[at], order[to]] = [order[to], order[at]];
  call('reorder_queue', { ids: order });
}

async function apply() {
  const saved = await call('set_settings', { settings: { ...settings, schedule: readForm() } });
  if (!saved) return false;
  settings = saved;
  dirty = false;
  $('saved').textContent = 'Saved';
  setTimeout(() => ($('saved').textContent = ''), 2000);
  return true;
}

async function init() {
  decorateIcons();
  $('days').replaceChildren(
    ...DAYS.map((day) => {
      const label = document.createElement('label');
      label.className = 'day';
      label.innerHTML = '<input type="checkbox"><span></span>';
      label.querySelector('span').textContent = day;
      return label;
    }),
  );
  for (const b of document.querySelectorAll('.tabs button')) b.addEventListener('click', () => showTab(b.dataset.tab));
  document.querySelector('.sched-body').addEventListener('input', () => {
    dirty = true;
    syncForm();
  });
  $('daily').addEventListener('change', () => {
    // Weekly with no day ticked would never start; begin with every day.
    const boxes = [...document.querySelectorAll('#days input')];
    if (!boxes.some((b) => b.checked)) boxes.forEach((b) => (b.checked = true));
    syncForm();
  });
  $('up').addEventListener('click', () => move(-1));
  $('down').addEventListener('click', () => move(1));
  $('remove').addEventListener('click', () => selected != null && call('remove_from_queue', { ids: [selected] }));
  $('start-now').addEventListener('click', () => call('start_queue'));
  $('stop-now').addEventListener('click', () => call('stop_queue'));
  $('apply').addEventListener('click', apply);
  $('close').addEventListener('click', async () => {
    if (!dirty || (await apply())) thisWindow().close();
  });
  closeOnEscape();

  settings = await invoke('get_settings');
  fillForm(settings.schedule);
  await onEvents((event) => {
    if (event.type === 'updated' || event.type === 'removed') loadQueue();
    else if (event.type === 'settings') {
      settings = event.settings;
      if (!dirty) fillForm(settings.schedule);
    }
  });
  await loadQueue();
}

init();
