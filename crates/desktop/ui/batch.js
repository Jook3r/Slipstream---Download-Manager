// Many links at once (IDM's "Download all links"): tick the ones to keep,
// by hand or by type.

import { $, CATEGORIES, call, categoryLabel, closeOnEscape, decorateIcons, invoke, params, setText, svg, thisWindow, toast } from './common.js';

const token = params.get('token');
let links = [];
const chosen = new Set();

function render() {
  const counts = new Map();
  for (const link of links) counts.set(link.category, (counts.get(link.category) ?? 0) + 1);
  const chips = $('chips');
  chips.replaceChildren(
    ...CATEGORIES.filter((c) => counts.has(c.id)).map((c) => {
      const inCategory = links.filter((l) => l.category === c.id);
      const allOn = inCategory.every((l) => chosen.has(l.url));
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.className = `chip${allOn ? ' on' : ''}`;
      chip.innerHTML = `<span class="cat cat-${c.id}">${svg(c.id)}</span><span class="label"></span><span class="n"></span>`;
      chip.querySelector('.label').textContent = c.label;
      chip.querySelector('.n').textContent = String(counts.get(c.id));
      chip.addEventListener('click', () => {
        for (const l of inCategory) {
          if (allOn) chosen.delete(l.url);
          else chosen.add(l.url);
        }
        render();
      });
      return chip;
    }),
  );
  for (const tr of $('rows').children) tr.querySelector('input').checked = chosen.has(tr.dataset.url);
  $('all').checked = links.length > 0 && chosen.size === links.length;
  $('all').indeterminate = chosen.size > 0 && chosen.size < links.length;
  setText($('count'), `${chosen.size} of ${links.length} selected`);
  $('start').disabled = $('later').disabled = chosen.size === 0;
}

function buildRows() {
  $('rows').replaceChildren(
    ...links.map((link) => {
      const tr = document.createElement('tr');
      tr.dataset.url = link.url;
      tr.innerHTML =
        '<td><input type="checkbox"></td><td><div class="name-cell"><span class="cat"></span><span class="fname"></span></div></td><td class="type"></td><td class="url"></td>';
      const cat = tr.querySelector('.cat');
      cat.classList.add(`cat-${link.category}`);
      cat.innerHTML = svg(link.category);
      tr.querySelector('.fname').textContent = link.fileName;
      tr.querySelector('.fname').title = link.text ?? link.fileName;
      tr.querySelector('.type').textContent = categoryLabel(link.category);
      tr.querySelector('.url').textContent = link.url;
      tr.querySelector('.url').title = link.url;
      tr.addEventListener('click', (e) => {
        if (e.target.tagName !== 'INPUT') tr.querySelector('input').click();
      });
      tr.querySelector('input').addEventListener('change', (e) => {
        if (e.target.checked) chosen.add(link.url);
        else chosen.delete(link.url);
        render();
      });
      return tr;
    }),
  );
}

async function submit(start) {
  const report = await call('confirm_batch', { token, urls: links.filter((l) => chosen.has(l.url)).map((l) => l.url), start });
  if (!report) return;
  if (report.skipped.length) toast(`${report.added.length} added, ${report.skipped.length} skipped (${report.skipped[0].reason})`);
  setTimeout(() => thisWindow().close(), report.skipped.length ? 1500 : 0);
}

async function init() {
  decorateIcons();
  links = (await invoke('pending_batch', { token })) ?? [];
  // Pages often link the same file twice.
  links = links.filter((l, k) => links.findIndex((m) => m.url === l.url) === k);
  // Start with everything except plain web pages and pictures ticked.
  for (const l of links) if (l.category !== 'other' && l.category !== 'images') chosen.add(l.url);
  if (!chosen.size) for (const l of links) chosen.add(l.url);
  thisWindow()
    .setTitle(`Download links: ${links.length} found`)
    .catch(() => {});
  buildRows();
  render();

  $('all').addEventListener('change', (e) => {
    chosen.clear();
    if (e.target.checked) for (const l of links) chosen.add(l.url);
    render();
  });
  $('start').addEventListener('click', () => submit(true));
  $('later').addEventListener('click', () => submit(false));
  $('cancel').addEventListener('click', async () => {
    await invoke('cancel_batch', { token });
    thisWindow().close();
  });
  closeOnEscape(() => invoke('cancel_batch', { token }));
}

init();
