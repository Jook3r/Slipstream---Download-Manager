// Download File Info: shown for each new link, as in IDM. The server is
// asked for the real name and size while the user looks at the window. A
// video stream (HLS or DASH) also gets a choice of quality.

import {
  $,
  CATEGORIES,
  call,
  categoryLabel,
  closeOnEscape,
  decorateIcons,
  fmtBytes,
  fmtDuration,
  folderFor,
  friendlyError,
  invoke,
  joinPath,
  needsFfmpeg,
  onEvents,
  params,
  qualityLabel,
  qualityShort,
  resizeWindow,
  safeFileName,
  splitPath,
  svg,
  thisWindow,
  variantExtension,
} from './common.js';

const token = params.get('token');
let settings = null;
let link = null;
let pathEdited = false;
let inspected = 0;
/** What the stream offers, when the link is a video stream. */
let stream = null;
let ffmpeg = null;

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
async function apply(info, fromServer) {
  $('category').value = info.category;
  showCategory(info.category);
  if (!pathEdited) $('path').value = info.suggestedPath;
  if (fromServer) {
    $('size').textContent = info.total != null ? fmtBytes(info.total) : 'Unknown size';
    $('resume').textContent = info.resumable ? 'Resume capability: Yes' : 'Resume capability: No';
    // A video found on a page is named after the page, as in IDM.
    const title = safeFileName(link?.media?.title);
    const ext = info.fileName.includes('.') ? info.fileName.split('.').pop() : '';
    if (link?.media?.kind === 'file' && title && ext && !pathEdited) {
      $('path').value = await invoke('suggest_path', { fileName: `${title}.${ext}` });
    }
  }
  checkPath();
}

function setStreamMode(on) {
  $('form').classList.toggle('stream', on);
  if (!on) {
    stream = null;
    $('ffmpeg').hidden = true;
  }
  fit();
}

async function inspect() {
  const url = $('url').value.trim();
  const ticket = ++inspected;
  $('size').textContent = 'Checking…';
  $('resume').textContent = '';
  showNotice('', '');
  const guess = await invoke('guess_link', { url });
  const hinted = ['hls', 'dash'].includes(link?.media?.kind);
  if (hinted || guess.stream) return inspectStream(url, ticket);
  setStreamMode(false);
  await apply(guess, false);
  try {
    const info = await invoke('inspect_link', { token, url });
    if (ticket !== inspected) return;
    // The server says it is a stream although the link does not look like one.
    if (info.stream) return inspectStream(url, ticket);
    await apply(info, true);
    if (info.webPage) {
      showNotice('error', friendlyError('web page instead of the file'));
    }
  } catch (err) {
    if (ticket !== inspected) return;
    $('size').textContent = 'Unknown size';
    showNotice('warn', `${friendlyError(err)} You can still try to download it.`, String(err));
  }
}

async function inspectStream(url, ticket) {
  setStreamMode(true);
  const quality = $('quality');
  quality.replaceChildren(new Option('Reading the video stream…', ''));
  quality.disabled = true;
  showCategory('video');
  $('category').value = 'video';
  try {
    const info = await invoke('inspect_stream', { token, url });
    if (ticket !== inspected) return;
    stream = info;
    quality.replaceChildren(...info.variants.map((v, i) => new Option(qualityLabel(v), String(i))));
    quality.disabled = info.variants.length < 2;
    await chooseQuality();
  } catch (err) {
    if (ticket !== inspected) return;
    quality.replaceChildren(new Option('Not available', ''));
    $('size').textContent = 'Unknown size';
    showNotice('error', friendlyError(err), String(err));
  }
}

const variant = () => stream?.variants[Number($('quality').value)] ?? null;

/** The video's name: the page's title, else the link's. */
function baseName() {
  const title = safeFileName(link?.media?.title);
  if (title) return title;
  let name = '';
  try {
    name = decodeURIComponent(splitPath(new URL($('url').value).pathname).name).replace(/\.[^.]*$/, '');
  } catch {
    // Not a URL; fall back to "video".
  }
  return /^(master|index|playlist|manifest|stream|main|chunklist.*|)$/i.test(name) ? 'video' : safeFileName(name);
}

async function chooseQuality() {
  const v = variant();
  if (!v) return;
  $('size').textContent = v.estimatedSize ? `About ${fmtBytes(v.estimatedSize)}` : 'Size not known';
  $('resume').textContent = stream.duration ? `Length ${fmtDuration(stream.duration)}` : '';
  const category = v.audioOnly ? 'music' : 'video';
  $('category').value = category;
  showCategory(category);
  const ext = variantExtension(v);
  if (pathEdited) {
    const { dir, name } = splitPath($('path').value);
    $('path').value = joinPath(dir, `${name.replace(/\.[^.]*$/, '')}.${ext}`);
  } else {
    $('path').value = await invoke('suggest_path', { fileName: `${baseName()}.${ext}` });
  }
  checkPath();
  showFfmpeg();
}

/** Says when ffmpeg is needed for the chosen quality, and offers to get it. */
function showFfmpeg() {
  const v = variant();
  const box = $('ffmpeg');
  const hide = !v || !needsFfmpeg(v) || Boolean(ffmpeg?.path);
  if (hide) {
    if (!box.hidden) {
      box.hidden = true;
      fit();
    }
    return;
  }
  box.hidden = false;
  box.className = `notice ffmpeg${v.separateAudio ? ' warn' : ''}`;
  $('ffmpeg-text').textContent = v.separateAudio
    ? 'This video’s picture and sound come separately. Slipstream needs ffmpeg (free) to join them into one file.'
    : 'Without ffmpeg (free), this video is saved as a .ts file, which VLC and most players play. With ffmpeg it becomes an MP4.';
  const installing = Boolean(ffmpeg?.installing);
  $('get-ffmpeg').hidden = !ffmpeg?.canInstall;
  $('get-ffmpeg').disabled = installing;
  $('choose-ffmpeg').hidden = installing;
  const progress = $('ffmpeg-progress');
  if (installing) {
    progress.textContent = ffmpeg.total
      ? `Downloading ffmpeg… ${Math.floor((ffmpeg.downloaded * 100) / ffmpeg.total)}%`
      : `Downloading ffmpeg… ${fmtBytes(ffmpeg.downloaded)}`;
  } else {
    progress.textContent = ffmpeg?.error ? friendlyError(ffmpeg.error) : '';
  }
  progress.title = ffmpeg?.error ?? '';
  progress.className = `hint${ffmpeg?.error && !installing ? ' error' : ''}`;
  fit();
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
    stream: null,
  };
  if (stream) {
    const v = variant();
    if (!v) return;
    choice.stream = { variant: v.key, quality: qualityShort(v) };
  }
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
  link = await invoke('pending_link', { token });
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
  $('quality').addEventListener('change', chooseQuality);
  $('get-ffmpeg').addEventListener('click', () => call('install_ffmpeg'));
  $('choose-ffmpeg').addEventListener('click', async () => {
    const status = await call('choose_ffmpeg');
    if (status) {
      ffmpeg = status;
      showFfmpeg();
    }
  });
  ffmpeg = await invoke('ffmpeg_status');
  await onEvents((event) => {
    if (event.type === 'ffmpeg') {
      ffmpeg = event.status;
      showFfmpeg();
    }
  });
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
