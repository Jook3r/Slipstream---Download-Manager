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

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === 'all') {
    downloadAll(tab, info);
  } else {
    const url = info.linkUrl || info.srcUrl;
    if (url && /^https?:/i.test(url)) handOver({ url, referrer: info.pageUrl });
  }
});

// ---------- the popup ----------

api.runtime.onMessage.addListener((message, _sender, reply) => {
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
  return false;
});
