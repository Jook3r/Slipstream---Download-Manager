// Slipstream Integration: hands downloads to the Slipstream app through its
// native messaging host (slipstream-bridge). If the app cannot be reached,
// the browser keeps the download, so nothing is ever lost.

const api = globalThis.browser ?? globalThis.chrome;
const HOST = 'io.github.jook3r.slipstream';

const DEFAULTS = {
  enabled: true,
  // IDM's default list of file types to take over, plus a few newer ones.
  extensions:
    '3gp 7z aac ace aif apk appimage arj asf avi bin bz2 deb dmg epub exe flac flv gz gzip img iso jar ' +
    'lzh m4a m4v mkv mov mp3 mp4 mpa mpe mpeg mpg msi msu ogg ogv opus pdf pkg qt rar rm rmvb rpm ' +
    'sea sit sitx tar tgz tif tiff txz wav webm wma wmv xz z zip zst',
  // Also take over any download bigger than this many MB (0: only listed types).
  minSizeMb: 0,
  // Show a "Download video" button over videos playing on pages.
  videoButton: true,
};

async function settings() {
  return { ...DEFAULTS, ...(await api.storage.local.get(Object.keys(DEFAULTS))) };
}

function sendToApp(message) {
  return api.runtime.sendNativeMessage(HOST, message);
}

async function noteProblem(problem) {
  await api.storage.session?.set({ problem: problem ?? null }).catch(() => {});
  await api.action.setBadgeText({ text: problem ? '!' : '' }).catch(() => {});
  if (problem) await api.action.setBadgeBackgroundColor({ color: '#d92d20' }).catch(() => {});
}

// ---------- deciding what to take over ----------

// Links clicked with Alt held stay with the browser (as in IDM).
const leaveToBrowser = new Map();
api.runtime.onMessage.addListener((message) => {
  if (message?.type === 'bypass' && typeof message.url === 'string') {
    leaveToBrowser.set(message.url, Date.now() + 15000);
  }
});

function bypassed(...urls) {
  const now = Date.now();
  for (const [url, until] of leaveToBrowser) if (until < now) leaveToBrowser.delete(url);
  return urls.some((url) => url && leaveToBrowser.has(url));
}

function nameOf(pathOrUrl) {
  if (!pathOrUrl) return '';
  try {
    if (/^https?:/i.test(pathOrUrl)) pathOrUrl = decodeURIComponent(new URL(pathOrUrl).pathname);
  } catch {
    // Keep the raw text.
  }
  return pathOrUrl.split(/[\\/]/).pop() ?? '';
}

const extensionOf = (name) => (name.includes('.') ? name.split('.').pop().toLowerCase() : '');

function shouldTakeOver(item, s) {
  const url = item.finalUrl || item.url;
  if (!s.enabled || item.incognito || item.byExtensionId) return false;
  if (!/^https?:/i.test(url) || !/^https?:/i.test(item.url)) return false;
  if (bypassed(item.url, item.finalUrl)) return false;
  const types = s.extensions.toLowerCase().split(/[\s,;.]+/).filter(Boolean);
  const ext = extensionOf(nameOf(item.filename) || nameOf(url));
  if (ext && types.includes(ext)) return true;
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize;
  return s.minSizeMb > 0 && size > s.minSizeMb * 1024 * 1024;
}

// ---------- handing over ----------

async function cookiesFor(url) {
  try {
    const cookies = await api.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join('; ');
  } catch {
    return '';
  }
}

/** Sends one link to the app; true if the app took it. */
async function handOver({ url, referrer, fileName, fileSize, mime }) {
  try {
    const reply = await sendToApp({
      type: 'download',
      url,
      referrer: referrer || undefined,
      fileName: fileName || undefined,
      fileSize: fileSize > 0 ? fileSize : undefined,
      mime: mime || undefined,
      cookies: await cookiesFor(url),
      userAgent: navigator.userAgent,
    });
    await noteProblem(reply?.ok ? null : reply?.error ?? 'Slipstream did not accept the link');
    return reply?.ok === true;
  } catch (err) {
    await noteProblem(`Can't reach Slipstream: ${err?.message ?? err}`);
    return false;
  }
}

const fromItem = (item) => ({
  url: item.url,
  referrer: item.referrer,
  fileName: nameOf(item.filename),
  fileSize: item.totalBytes > 0 ? item.totalBytes : item.fileSize,
  mime: item.mime,
});

async function forget(id) {
  try {
    await api.downloads.cancel(id);
  } catch {
    // Already gone.
  }
  try {
    await api.downloads.erase({ id });
  } catch {
    // Already gone.
  }
}

// Chrome-family browsers ask extensions for a file name before saving
// anything, which is the cleanest moment to take a download over.
if (api.downloads.onDeterminingFilename) {
  api.downloads.onDeterminingFilename.addListener((item, suggest) => {
    (async () => {
      const keepInBrowser = !shouldTakeOver(item, await settings()) || !(await handOver(fromItem(item)));
      if (keepInBrowser) return suggest();
      await forget(item.id);
      try {
        suggest();
      } catch {
        // The download is already cancelled.
      }
    })();
    return true;
  });
} else {
  // Firefox: hold the download while asking the app, then cancel or resume it.
  api.downloads.onCreated.addListener(async (item) => {
    if (!shouldTakeOver(item, await settings())) return;
    await api.downloads.pause(item.id).catch(() => {});
    if (await handOver(fromItem(item))) {
      await forget(item.id);
      await api.downloads.removeFile?.(item.id).catch(() => {});
    } else {
      await api.downloads.resume(item.id).catch(() => {});
    }
  });
}

// ---------- videos playing on pages ----------

// Media files smaller than this are sounds and previews, not videos.
const MIN_MEDIA_BYTES = 1 << 20;
// Store rules forbid extensions from downloading videos from these sites.
const BLOCKED_HOSTS = /(^|\.)(youtube\.com|youtube-nocookie\.com|googlevideo\.com|ytimg\.com)$/i;
// Pieces of a stream, which are found through its playlist instead.
const STREAM_PIECE = /\.(ts|m4s|m4f|m4v?s|cmf[va]|aac|vtt|webvtt)$/i;

const mediaKey = (tabId) => `media:${tabId}`;

/** The videos found on a tab, kept in session storage because the browser
 * stops this worker when it is idle. */
async function tabMedia(tabId) {
  const stored = await api.storage.session?.get(mediaKey(tabId)).catch(() => null);
  return stored?.[mediaKey(tabId)] ?? { items: [], variantUrls: [] };
}

async function saveTabMedia(tabId, media) {
  await api.storage.session?.set({ [mediaKey(tabId)]: media }).catch(() => {});
}

function header(details, name) {
  return details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value ?? '';
}

/** What kind of video a response is, if any. */
function classify(details) {
  const url = new URL(details.url);
  if (BLOCKED_HOSTS.test(url.hostname)) return null;
  const type = header(details, 'content-type').split(';')[0].trim().toLowerCase();
  const path = url.pathname.toLowerCase();
  if (type.includes('mpegurl') || path.endsWith('.m3u8')) return { kind: 'hls' };
  if (type === 'application/dash+xml' || path.endsWith('.mpd')) return { kind: 'dash' };
  if (!/^(video|audio)\//.test(type) || type === 'video/mp2t' || type.startsWith('video/iso.segment') || STREAM_PIECE.test(path)) {
    return null;
  }
  // A player usually asks for a range: the size is after the slash.
  const range = header(details, 'content-range').match(/\/(\d+)\s*$/);
  const size = Number(range?.[1] ?? header(details, 'content-length')) || 0;
  return size >= MIN_MEDIA_BYTES ? { kind: 'file', size, mime: type } : null;
}

/** A master playlist lists qualities; the playlists it lists are not shown
 * separately. Returns those, or null if this is not a master playlist. */
async function hlsVariants(url) {
  try {
    const text = await (await fetch(url, { credentials: 'include' })).text();
    if (!text.includes('#EXT-X-STREAM-INF')) return null;
    const lines = text.split(/\r?\n/).map((l) => l.trim());
    const out = [];
    lines.forEach((line, i) => {
      if (line.startsWith('#EXT-X-STREAM-INF') && lines[i + 1] && !lines[i + 1].startsWith('#')) out.push(new URL(lines[i + 1], url).href);
      const audio = line.startsWith('#EXT-X-MEDIA') && line.match(/URI="([^"]+)"/);
      if (audio) out.push(new URL(audio[1], url).href);
    });
    return out;
  } catch {
    return null;
  }
}

const noQuery = (url) => url.split(/[?#]/)[0];

// One update at a time per tab: a master playlist and its quality playlists
// arrive together, and each update reads and rewrites the tab's list.
const updates = new Map();

function found(tabId, entry) {
  const next = (updates.get(tabId) ?? Promise.resolve()).then(() => addMedia(tabId, entry)).catch(() => {});
  updates.set(tabId, next);
  next.then(() => updates.get(tabId) === next && updates.delete(tabId));
}

async function addMedia(tabId, entry) {
  const media = await tabMedia(tabId);
  if (media.items.some((m) => m.url === entry.url)) return;
  if (media.variantUrls.includes(noQuery(entry.url))) return;
  if (entry.kind === 'hls') {
    const variants = await hlsVariants(entry.url);
    if (variants) {
      const known = new Set(variants.map(noQuery));
      media.variantUrls.push(...known);
      // A quality playlist seen before its master gives way to the master.
      media.items = media.items.filter((m) => !known.has(noQuery(m.url)));
    }
  }
  media.items.push({ ...entry, at: Date.now() });
  // No more than a page would sensibly offer; the oldest go first.
  media.items = media.items.slice(-20);
  await saveTabMedia(tabId, media);
  api.tabs.sendMessage(tabId, { type: 'media-found', count: media.items.length }).catch(() => {});
}

api.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (details.tabId < 0 || details.statusCode >= 400) return;
    const entry = classify(details);
    if (entry) found(details.tabId, { url: details.url, ...entry });
  },
  { urls: ['<all_urls>'], types: ['media', 'xmlhttprequest', 'other'] },
  ['responseHeaders'],
);

api.tabs.onUpdated.addListener((tabId, change) => {
  // A new page (not a change of address inside the same page).
  if (change.status === 'loading' && change.url) api.storage.session?.remove(mediaKey(tabId)).catch(() => {});
});
api.tabs.onRemoved.addListener((tabId) => api.storage.session?.remove(mediaKey(tabId)).catch(() => {}));

/** How a found video is listed: "HLS stream · master.m3u8", "movie.mp4 · 45 MB". */
function describe(entry) {
  const name = decodeURIComponent(new URL(entry.url).pathname.split('/').pop() || entry.url);
  if (entry.kind === 'hls') return { title: 'Video stream (HLS)', detail: name };
  if (entry.kind === 'dash') return { title: 'Video stream (DASH)', detail: name };
  const mb = entry.size / (1024 * 1024);
  return { title: name, detail: `${entry.mime || 'video'} · ${mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb.toFixed(1)} MB`}` };
}

/** Streams first (they carry every quality), then the biggest files. */
function listMedia(media) {
  return [...media.items]
    .sort((a, b) => (a.kind === 'file') - (b.kind === 'file') || (b.size ?? 0) - (a.size ?? 0) || b.at - a.at)
    .map((entry) => ({ ...entry, ...describe(entry) }));
}

/** Hands a found video to Slipstream, named after the page. */
async function downloadMedia(tab, entry) {
  try {
    const reply = await sendToApp({
      type: 'media',
      url: entry.url,
      kind: entry.kind,
      title: tab?.title || undefined,
      referrer: tab?.url || undefined,
      fileSize: entry.size || undefined,
      cookies: await cookiesFor(entry.url),
      userAgent: navigator.userAgent,
    });
    await noteProblem(reply?.ok ? null : reply?.error ?? 'Slipstream did not accept the video');
    return reply?.ok === true;
  } catch (err) {
    await noteProblem(`Can't reach Slipstream: ${err?.message ?? err}`);
    return false;
  }
}

// ---------- right-click menus ----------

async function setUpMenus() {
  await api.contextMenus.removeAll();
  api.contextMenus.create({ id: 'link', title: 'Download with Slipstream', contexts: ['link'] });
  api.contextMenus.create({ id: 'media', title: 'Download with Slipstream', contexts: ['video', 'audio', 'image'] });
  api.contextMenus.create({ id: 'all', title: 'Download all links with Slipstream', contexts: ['page', 'selection'] });
}
api.runtime.onInstalled.addListener(setUpMenus);
api.runtime.onStartup.addListener(setUpMenus);

/** Runs in the page: the links on it, or only those in the selection. */
function collectLinks(selectionOnly) {
  const selection = getSelection();
  const inSelection = (el) => !selectionOnly || (selection.rangeCount > 0 && selection.containsNode(el, true));
  const links = [];
  for (const a of document.querySelectorAll('a[href]')) {
    if (inSelection(a)) links.push({ url: a.href, text: a.textContent.trim().replace(/\s+/g, ' ').slice(0, 200) });
  }
  for (const el of document.querySelectorAll('video[src], audio[src], source[src]')) {
    if (inSelection(el)) links.push({ url: el.src, text: '' });
  }
  return links.filter((l) => /^https?:/i.test(l.url));
}

async function downloadAll(tab, info) {
  const results = await api.scripting.executeScript({
    target: { tabId: tab.id, allFrames: true },
    func: collectLinks,
    args: [Boolean(info.selectionText)],
  });
  const seen = new Set();
  const links = results
    .flatMap((r) => r.result ?? [])
    .filter((l) => !seen.has(l.url) && seen.add(l.url));
  if (!links.length) return;
  const cookiesByOrigin = new Map();
  for (const link of links) {
    const origin = new URL(link.url).origin;
    if (!cookiesByOrigin.has(origin)) cookiesByOrigin.set(origin, await cookiesFor(link.url));
    link.cookies = cookiesByOrigin.get(origin) || undefined;
  }
  try {
    const reply = await sendToApp({ type: 'downloadAll', links, referrer: info.pageUrl, userAgent: navigator.userAgent });
    await noteProblem(reply?.ok ? null : reply?.error);
  } catch (err) {
    await noteProblem(`Can't reach Slipstream: ${err?.message ?? err}`);
  }
}

api.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === 'all') {
    downloadAll(tab, info);
    return;
  }
  const url = info.linkUrl || info.srcUrl;
  if (url && /^https?:/i.test(url)) {
    handOver({ url, referrer: info.pageUrl });
  } else if (info.mediaType === 'video' || info.mediaType === 'audio') {
    // A player fed by a stream (its source is a blob:): offer the stream.
    const [best] = listMedia(await tabMedia(tab.id));
    if (best) downloadMedia(tab, best);
  }
});

// ---------- the popup ----------

api.runtime.onMessage.addListener((message, sender, reply) => {
  if (message?.type === 'status') {
    sendToApp({ type: 'ping' })
      .then((answer) => reply({ ok: true, answer }))
      .catch((err) => reply({ ok: false, error: String(err?.message ?? err) }));
    return true;
  }
  if (message?.type === 'settings') {
    settings().then(reply);
    return true;
  }
  if (message?.type === 'activate') {
    sendToApp({ type: 'activate' }).catch(() => {});
  }
  if (message?.type === 'media-list') {
    // From the popup (which names the tab) or a page's button (the sender).
    const tabId = message.tabId ?? sender.tab?.id;
    Promise.all([tabMedia(tabId), settings()]).then(([media, s]) => reply({ items: listMedia(media), videoButton: s.videoButton }));
    return true;
  }
  if (message?.type === 'download-media') {
    const tabId = message.tabId ?? sender.tab?.id;
    (async () => {
      const entry = (await tabMedia(tabId)).items.find((m) => m.url === message.url);
      const tab = await api.tabs.get(tabId).catch(() => sender.tab);
      reply({ ok: entry ? await downloadMedia(tab, entry) : false });
    })();
    return true;
  }
  return false;
});
