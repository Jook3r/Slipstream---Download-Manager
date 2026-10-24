// Shared by every Slipstream window: icons, formatting, and talking to the
// app. Anything that came from the network is shown with textContent.

export const tauri = window.__TAURI__;
export const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
export const $ = (id) => document.getElementById(id);
export const isMac = /Mac/.test(navigator.platform);

export const ICONS = {
  // Toolbar, after IDM's newer flat theme.
  addUrl: '<circle cx="12" cy="12" r="9.5"/><path d="M12 7.5v9M7.5 12h9"/>',
  resume: '<circle cx="12" cy="12" r="9.5"/><path d="M12 7v9M8 12.5l4 4 4-4"/>',
  stop: '<circle cx="12" cy="12" r="9.5"/><rect x="8.8" y="8.8" width="6.4" height="6.4" rx="1"/>',
  stopAll: '<path d="M8.5 12.5V5.8a1.4 1.4 0 0 1 2.8 0V11M11.3 10.5V4.4a1.4 1.4 0 0 1 2.8 0v6.1M14.1 10.6V5.8a1.4 1.4 0 0 1 2.8 0v7.4M16.9 9.8a1.4 1.4 0 0 1 2.8 0v4.9a6.8 6.8 0 0 1-6.8 6.8h-1a6.6 6.6 0 0 1-5-2.3l-3-3.6a1.5 1.5 0 0 1 2.2-2l2.4 2.2"/>',
  delete: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
  deleteDone: '<path d="M4 7h16M6 7l1 13h10l1-13M9 7V4h6v3"/><path d="M9.2 13.6l2 2 3.8-4"/>',
  options: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 0 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 0 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 0 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 0 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  startQueue: '<path d="M3.5 6h11M3.5 11h7M3.5 16h6"/><path d="M14.5 12.5v8l6.5-4z"/>',
  stopQueue: '<path d="M3.5 6h11M3.5 11h7M3.5 16h6"/><path d="M15.5 13v7M19.5 13v7"/>',
  // Small icons.
  play: '<path d="M7 4.8v14.4a.8.8 0 0 0 1.2.7l11.3-7.2a.8.8 0 0 0 0-1.4L8.2 4.1a.8.8 0 0 0-1.2.7z"/>',
  pause: '<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>',
  trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  folderOpen: '<path d="M3 8V6a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v2"/><path d="M3 19l2.6-8.2a1 1 0 0 1 1-.8H21a.6.6 0 0 1 .6.8L19 19H3z"/>',
  search: '<circle cx="11" cy="11" r="6"/><path d="M20 20l-4.5-4.5"/>',
  download: '<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>',
  open: '<path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.66 0l3-3a4 4 0 0 0-5.66-5.66l-1 1M14 10a4 4 0 0 0-5.66 0l-3 3a4 4 0 0 0 5.66 5.66l1-1"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8h.01"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  chevron: '<path d="M9 6l6 6-6 6"/>',
  check: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
  all: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  unfinished: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  finished: '<circle cx="12" cy="12" r="9"/><path d="M8 12.5l2.7 2.7L16.2 9.5"/>',
  downloading: '<circle cx="12" cy="12" r="9"/><path d="M12 7.5v9M8.5 13l3.5 3.5 3.5-3.5"/>',
  queued: '<path d="M4 6h11M4 12h7M4 18h6"/><circle cx="17" cy="15" r="4"/><path d="M17 13.2v1.8l1.2 1.2"/>',
  video: '<rect x="3" y="6" width="13" height="12" rx="2"/><path d="M16 10.5l5-3v9l-5-3"/>',
  music: '<path d="M9 18V6l11-2v12"/><circle cx="6.5" cy="18" r="2.5"/><circle cx="17.5" cy="16" r="2.5"/>',
  documents: '<path d="M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 16h6"/>',
  compressed: '<rect x="4" y="3" width="16" height="18" rx="2"/><path d="M12 3v2M12 7v2M12 11v2"/><rect x="10.5" y="14" width="3" height="4" rx="1"/>',
  programs: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M6.5 6.5h.01M9.5 6.5h.01"/>',
  images: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="2"/><path d="M21 16l-5-5-9 9"/>',
  other: '<path d="M6 3h8l4 4v14H6zM14 3v4h4"/>',
  complete: '<path d="M3 8V6a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v2"/><path d="M3 19l2.6-8.2a1 1 0 0 1 1-.8H21a.6.6 0 0 1 .6.8L19 19H3z"/><path d="M10 14.5l2 2 4-4"/>',
};

export const svg = (name, cls = 'ic') => `<svg class="${cls}" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] ?? ''}</svg>`;

/** Fills every `[data-icon]` element with its icon. */
export function decorateIcons(root = document) {
  for (const el of root.querySelectorAll('[data-icon]')) el.insertAdjacentHTML('afterbegin', svg(el.dataset.icon));
  if (isMac) {
    for (const kbd of root.querySelectorAll('kbd')) if (kbd.textContent === 'Ctrl') kbd.textContent = '⌘';
    for (const el of root.querySelectorAll('[title*="Ctrl+"]')) el.title = el.title.replace('Ctrl+', '⌘');
  }
}

/** IDM's categories, in IDM's order; `folder` is the sub-folder name. */
export const CATEGORIES = [
  { id: 'compressed', label: 'Compressed', folder: 'Compressed' },
  { id: 'documents', label: 'Documents', folder: 'Documents' },
  { id: 'music', label: 'Music', folder: 'Music' },
  { id: 'programs', label: 'Programs', folder: 'Programs' },
  { id: 'video', label: 'Video', folder: 'Video' },
  { id: 'images', label: 'Images', folder: 'Images' },
  { id: 'other', label: 'General', folder: null },
];
export const categoryLabel = (id) => CATEGORIES.find((c) => c.id === id)?.label ?? 'General';

export const STATUS_LABEL = {
  downloading: 'Downloading',
  queued: 'Queued',
  paused: 'Paused',
  completed: 'Complete',
  failed: 'Error',
};

// ---------- paths ----------

const sepOf = (path) => (path.includes('\\') && !path.includes('/') ? '\\' : '/');

export function splitPath(path) {
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  return i < 0 ? { dir: '', name: path } : { dir: path.slice(0, i), name: path.slice(i + 1) };
}

export function joinPath(dir, name) {
  if (!dir) return name;
  const sep = sepOf(dir);
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
}

/** Where a category's files go, mirroring `Settings::folder_for`. */
export function folderFor(settings, category) {
  const remembered = settings.categoryDirs?.[category];
  if (remembered) return remembered;
  const sub = CATEGORIES.find((c) => c.id === category)?.folder;
  return settings.useCategories && sub ? joinPath(settings.downloadDir, sub) : settings.downloadDir;
}

// ---------- formatting ----------

export function fmtBytes(n) {
  if (n == null) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${n.toFixed(2)} ${units[i]}`;
}

const grouped = new Intl.NumberFormat();
export const fmtExactBytes = (n) => `${grouped.format(Math.round(n))} Bytes`;

export const fmtSpeed = (bytesPerSec, unit = '/s') =>
  bytesPerSec >= 1 ? `${fmtBytes(Math.round(bytesPerSec))}${unit}` : '';

/** "6 sec", "5 min 44 sec", "2 hours 3 min", as IDM writes it. */
export function fmtEta(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '';
  const s = Math.max(1, Math.round(seconds));
  if (s < 60) return `${s} sec`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${s % 60} sec`;
  const h = Math.floor(s / 3600);
  if (h > 99) return 'more than 99 hours';
  return `${h} hour${h > 1 ? 's' : ''} ${Math.floor((s % 3600) / 60)} min`;
}

/** A video's length: 4:05, or 1:02:03. */
export function fmtDuration(seconds) {
  const s = Math.round(seconds);
  const pad = (n) => String(n).padStart(2, '0');
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

/** One quality of a video stream: "720p (1280×720) · 2.1 Mbit/s · about 160 MB". */
export function qualityLabel(v) {
  const parts = [];
  if (v.height) parts.push(v.width ? `${v.height}p (${v.width}×${v.height})` : `${v.height}p`);
  else if (v.audioOnly) parts.push('Audio only');
  if (v.bandwidth) {
    parts.push(v.bandwidth >= 1e6 ? `${(v.bandwidth / 1e6).toFixed(1)} Mbit/s` : `${Math.round(v.bandwidth / 1000)} kbit/s`);
  }
  if (v.estimatedSize) parts.push(`about ${fmtBytes(v.estimatedSize)}`);
  return parts.join(' · ') || 'Standard quality';
}

/** The short form kept with the download: "720p", "Audio only". */
export const qualityShort = (v) => (v.height ? `${v.height}p` : v.audioOnly ? 'Audio only' : '');

/** The extension the finished video gets (as in the engine's Variant). */
export const variantExtension = (v) => (v.container === 'webm' ? 'webm' : v.audioOnly ? 'm4a' : 'mp4');

/** Whether ffmpeg is needed to end up with a normal video file. */
export const needsFfmpeg = (v) => v.separateAudio || v.container === 'ts';

/** A page title made safe as a file name. */
export function safeFileName(text) {
  return (text ?? '')
    .replace(/[<>:"/\\|?*\u0000-\u001f]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/[. ]+$/, '')
    .slice(0, 120);
}

const dateFmt = new Intl.DateTimeFormat(undefined, {
  month: 'short',
  day: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
});
export const fmtDate = (ms) => (ms ? dateFmt.format(new Date(ms)) : '');

export const percent = (item) => (item.total > 0 ? Math.min(100, (item.downloaded / item.total) * 100) : null);
export const fmtPercent = (p) => `${p.toFixed(2)}%`;

/** A plain-language summary of an error from the app; the full text can go in a tooltip. */
export function friendlyError(error) {
  const text = String(error ?? '').trim();
  const rules = [
    [/web page instead of the file/i, 'The server sent a web page instead of the file. The link has probably expired: click it again on the website to get a fresh one.'],
    [/forcibly closed|connection reset|reset by peer|connection closed|broken pipe|unexpected eof/i, 'The server closed the connection.'],
    [/timed? ?out/i, 'The server took too long to answer.'],
    [/dns|failed to lookup|no such host|name or service not known|nodename nor servname/i, "Couldn't find the server. Check the address and your internet connection."],
    [/certificate|\btls\b|\bssl\b|handshake/i, "Couldn't make a secure connection to the server."],
    [/refused/i, 'The server refused the connection.'],
    [/\b404\b/, "The file isn't on the server (404)."],
    [/\b(401|403)\b/, 'The server refused access. The link may have expired or need you to sign in.'],
    [/\b(500|502|503|504)\b/, 'The server had a problem. Try again in a little while.'],
    [/no space|disk full|not enough space/i, 'The disk is full.'],
    [/permission denied|access is denied/i, "Slipstream isn't allowed to save in that folder."],
  ];
  for (const [pattern, summary] of rules) if (pattern.test(text)) return summary;
  const sentence = text.charAt(0).toUpperCase() + text.slice(1);
  return sentence.length > 160 ? `${sentence.slice(0, 157)}…` : sentence;
}

export const setText = (el, text) => {
  if (el.textContent !== text) el.textContent = text;
};

// ---------- talking to the app ----------

let toastTimer;
export function toast(message, isError = false) {
  let el = document.getElementById('toast');
  if (!el) {
    el = document.createElement('div');
    el.id = 'toast';
    el.className = 'toast';
    el.setAttribute('role', 'status');
    document.body.append(el);
  }
  el.textContent = message;
  el.classList.toggle('error', isError);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), isError ? 6000 : 2500);
}

/** Invokes a command, showing any error; resolves to `undefined` on error. */
export async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (err) {
    toast(typeof err === 'string' ? err : err?.message ?? String(err), true);
    return undefined;
  }
}

export const onEvents = (handler) => tauri.event.listen('slipstream', (e) => handler(e.payload));

export const thisWindow = () => tauri.window.getCurrentWindow();

export async function resizeWindow(width, height) {
  try {
    await thisWindow().setSize(new tauri.dpi.LogicalSize(width, height));
  } catch {
    // Not fatal: the page still works at its old size.
  }
}

/** Closes the window on Escape, as dialogs do. */
export function closeOnEscape(before) {
  document.addEventListener('keydown', async (e) => {
    if (e.key === 'Escape') {
      await before?.();
      thisWindow().close();
    }
  });
}

export const params = new URLSearchParams(location.search);
