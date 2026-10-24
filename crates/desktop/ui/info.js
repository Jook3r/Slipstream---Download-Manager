// Download File Info: shown for each new link, as in IDM. The server is
// asked for the real name and size while the user looks at the window.

import {
  $,
  CATEGORIES,
  call,
  categoryLabel,
  closeOnEscape,
  decorateIcons,
  fmtBytes,
  folderFor,
  friendlyError,
  invoke,
  joinPath,
  params,
  resizeWindow,
  splitPath,
  svg,
  thisWindow,
} from './common.js';

const token = params.get('token');
let settings = null;
let pathEdited = false;
let inspected = 0;

const WIDTH = 620;

/** Grows or shrinks the window to fit its contents, so nothing is cut off. */
function fit() {
  requestAnimationFrame(() => {
    const height = Math.ceil($('form').getBoundingClientRect().height) + 24;
    resizeWindow(WIDTH, Math.min(height, 560));
  });
}

/** A problem with the link: a short summary, the full text on request. */
function showNotice(kind, summary, details = '', canRetry = true) {
  $('notice').hidden = !summary;
  $('notice').className = `notice ${kind}`;
  $('notice-text').textContent = summary;
  $('notice-text').title = details;
  $('notice-details').textContent = details;
  $('notice-details').hidden = true;
  $('details-toggle').hidden = !details;
  $('details-toggle').textContent = 'Show details';
  $('retry').hidden = !canRetry;
  fit();
}

function showCategory(category) {
  const icon = $('icon');
  if (icon.dataset.cat !== category) {
    icon.dataset.cat = category;
    icon.className = `big-icon cat cat-${category}`;
    icon.innerHTML = svg(category, 'ic');
  }
  $('remember-label').textContent = `Remember this path for “${categoryLabel(category)}” category`;
}

/** Shows what is known about the link: a guess first, then the server's answer. */
function apply(info, fromServer) {
  $('category').value = info.category;
  showCategory(info.category);
  if (!pathEdited) $('path').value = info.suggestedPath;
  if (fromServer) {
    $('size').textContent = info.total != null ? fmtBytes(info.total) : 'Unknown size';
    $('resume').textContent = info.resumable ? 'Resume capability: Yes' : 'Resume capability: No';
  }
  checkPath();
}

async function inspect() {
  const url = $('url').value.trim();
  const ticket = ++inspected;
  $('size').textContent = 'Checking…';
  $('resume').textContent = '';
  apply(await invoke('guess_link', { url }), false);
  showNotice('', '');
  try {
    const info = await invoke('inspect_link', { token, url });
    if (ticket !== inspected) return;
    apply(info, true);
    if (info.webPage) {
      showNotice('error', friendlyError('web page instead of the file'));
    }
  } catch (err) {
    if (ticket !== inspected) return;
    $('size').textContent = 'Unknown size';
    showNotice('warn', `${friendlyError(err)} You can still try to download it.`, String(err));
  }
}

async function checkPath() {
  const path = $('path').value.trim();
  const exists = Boolean(path) && (await invoke('path_exists', { path }));
  if ($('exists').hidden === exists) {
    $('exists').hidden = !exists;
    fit();
  }
}

async function submit(start) {
  const choice = {
    url: $('url').value.trim(),
    path: $('path').value.trim(),
    category: $('category').value,
    description: $('description').value,
    start,
    rememberFolder: $('remember').checked,
  };
  const id = await call('confirm_link', { token, choice });
  if (id != null) thisWindow().close();
}

async function cancel() {
  await invoke('cancel_link', { token });
  thisWindow().close();
}

async function init() {
  decorateIcons();
  for (const c of CATEGORIES) {
    const option = document.createElement('option');
    option.value = c.id;
    option.textContent = c.label;
    $('category').append(option);
  }
  settings = await invoke('get_settings');
  const link = await invoke('pending_link', { token });
  if (!link) {
    showNotice('error', 'This link is no longer waiting. Close this window and try again.', '', false);
    for (const b of document.querySelectorAll('button[type=submit]')) b.disabled = true;
    return;
  }
  $('url').value = link.url;
  if (link.fileSize) $('size').textContent = fmtBytes(link.fileSize);

  $('form').addEventListener('submit', (e) => {
    e.preventDefault();
    submit(e.submitter?.value !== 'later');
  });
  $('cancel').addEventListener('click', cancel);
  $('retry').addEventListener('click', inspect);
  $('details-toggle').addEventListener('click', () => {
    const details = $('notice-details');
    details.hidden = !details.hidden;
    $('details-toggle').textContent = details.hidden ? 'Show details' : 'Hide details';
    fit();
  });
  closeOnEscape(() => invoke('cancel_link', { token }));
  $('url').addEventListener('change', () => {
    pathEdited = false;
    inspect();
  });
  $('category').addEventListener('change', () => {
    const category = $('category').value;
    const { name } = splitPath($('path').value);
    $('path').value = joinPath(folderFor(settings, category), name);
    showCategory(category);
    checkPath();
  });
  $('path').addEventListener('input', () => {
    pathEdited = true;
    checkPath();
  });
  $('browse').addEventListener('click', async () => {
    const chosen = await call('choose_save_path', { path: $('path').value });
    if (chosen) {
      $('path').value = chosen;
      pathEdited = true;
      checkPath();
    }
  });

  await inspect();
  $('start').focus();
}

init();
