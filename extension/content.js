// Runs in every page: Alt+click leaves a download to the browser, and a
// "Download video" button appears over videos the page is playing, as in
// IDM.

const api = globalThis.browser ?? globalThis.chrome;

// Alt+click on a link leaves that download to the browser.
document.addEventListener(
  'click',
  (e) => {
    if (!e.altKey) return;
    const link = e.target.closest?.('a[href]');
    if (!link) return;
    api.runtime.sendMessage({ type: 'bypass', url: link.href })?.catch?.(() => {});
  },
  true,
);

// ---------- the "Download video" button ----------

(() => {
  // Videos smaller than this are previews and adverts.
  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 110;

  let media = [];
  let enabled = true;
  let host = null;
  let button = null;
  let menu = null;
  let target = null;
  let hideTimer = 0;
  let pending = false;
  let pointer = { x: -1, y: -1 };

  async function refresh() {
    const answer = await api.runtime.sendMessage({ type: 'media-list' }).catch(() => null);
    media = answer?.items ?? [];
    enabled = answer?.videoButton !== false;
    if (!media.length || !enabled) hide();
  }

  api.runtime.onMessage.addListener((message) => {
    if (message?.type === 'media-found') refresh();
  });
  api.storage.onChanged.addListener((changes) => {
    if ('videoButton' in changes) refresh();
  });

  const STYLE = `
    :host { all: initial; }
    .wrap { font: 12px/1.3 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; display: flex; flex-direction: column; align-items: flex-end; gap: 4px; }
    .btn {
      display: inline-flex; align-items: center; gap: 6px;
      padding: 6px 10px; border: 0; border-radius: 6px;
      background: #1f6feb; color: #fff; font: inherit; font-weight: 600;
      box-shadow: 0 2px 10px rgba(0, 0, 0, 0.35); cursor: pointer;
    }
    .btn:hover { background: #2b7bff; }
    .btn svg { width: 14px; height: 14px; fill: none; stroke: currentColor; stroke-width: 2.2; stroke-linecap: round; stroke-linejoin: round; }
    .menu {
      min-width: 240px; max-width: 360px; padding: 4px; border-radius: 8px;
      background: #fff; color: #1b2130; box-shadow: 0 8px 28px rgba(0, 0, 0, 0.35);
    }
    .menu[hidden] { display: none; }
    .item { display: block; width: 100%; padding: 6px 8px; border: 0; border-radius: 5px; background: none; color: inherit; font: inherit; text-align: left; cursor: pointer; }
    .item:hover, .item:focus-visible { background: #e8f0ff; outline: none; }
    .item b { display: block; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .item span { display: block; color: #5f6b7d; font-size: 11px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  `;
  const ARROW = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v11M7 10l5 5 5-5M5 20h14"/></svg>';

  function ensureHost() {
    if (host) return;
    host = document.createElement('slipstream-video-button');
    host.style.cssText = 'all: initial; position: fixed; z-index: 2147483647; top: 0; left: 0; display: none;';
    const shadow = host.attachShadow({ mode: 'closed' });
    shadow.innerHTML = `<style>${STYLE}</style><div class="wrap"><button class="btn" type="button">${ARROW}<span>Download video</span></button><div class="menu" hidden></div></div>`;
    button = shadow.querySelector('.btn');
    menu = shadow.querySelector('.menu');
    button.addEventListener('click', (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (media.length === 1) send(media[0]);
      else toggleMenu();
    });
    host.addEventListener('mouseenter', () => clearTimeout(hideTimer));
    host.addEventListener('mouseleave', () => scheduleHide());
    // Clicks on the button must not reach the player (pause, fullscreen).
    for (const type of ['mousedown', 'mouseup', 'pointerdown', 'pointerup', 'dblclick']) {
      host.addEventListener(type, (e) => e.stopPropagation());
    }
  }

  function toggleMenu() {
    if (!menu.hidden) {
      menu.hidden = true;
      return;
    }
    menu.replaceChildren(
      ...media.map((entry) => {
        const item = document.createElement('button');
        item.type = 'button';
        item.className = 'item';
        const title = document.createElement('b');
        title.textContent = entry.title;
        const detail = document.createElement('span');
        detail.textContent = entry.detail;
        item.append(title, detail);
        item.addEventListener('click', (e) => {
          e.stopPropagation();
          send(entry);
        });
        return item;
      }),
    );
    menu.hidden = false;
    place();
  }

  async function send(entry) {
    menu.hidden = true;
    const label = button.querySelector('span');
    label.textContent = 'Opening Slipstream…';
    const answer = await api.runtime.sendMessage({ type: 'download-media', url: entry.url }).catch(() => null);
    label.textContent = answer?.ok ? 'Sent to Slipstream' : "Couldn't reach Slipstream";
    setTimeout(() => {
      label.textContent = 'Download video';
    }, 2500);
  }

  /** The video under the pointer, if it is big enough to be the main one. */
  function videoAt(x, y) {
    for (const video of document.querySelectorAll('video')) {
      const r = video.getBoundingClientRect();
      if (r.width < MIN_WIDTH || r.height < MIN_HEIGHT) continue;
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return video;
    }
    return null;
  }

  /** At the video's top right corner, inside the window. */
  function place() {
    if (!target || !host) return;
    // In fullscreen only the fullscreen element is drawn, so go inside it.
    const full = document.fullscreenElement;
    const parent = full && full.tagName !== 'VIDEO' ? full : document.documentElement;
    if (host.parentNode !== parent) parent.append(host);
    const r = target.getBoundingClientRect();
    const width = host.getBoundingClientRect().width || 140;
    host.style.top = `${Math.max(4, r.top + 10)}px`;
    host.style.left = `${Math.max(4, Math.min(innerWidth - width - 4, r.right - width - 10))}px`;
  }

  function show(video) {
    ensureHost();
    clearTimeout(hideTimer);
    target = video;
    host.style.display = 'block';
    place();
  }

  function hide() {
    clearTimeout(hideTimer);
    if (!host) return;
    host.style.display = 'none';
    menu.hidden = true;
    target = null;
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(hide, 1200);
  }

  function update() {
    pending = false;
    if (!media.length || !enabled) return;
    const video = videoAt(pointer.x, pointer.y);
    if (video) show(video);
    else if (target && menu?.hidden) scheduleHide();
  }

  document.addEventListener(
    'mousemove',
    (e) => {
      pointer = { x: e.clientX, y: e.clientY };
      if (!pending && media.length && enabled) {
        pending = true;
        requestAnimationFrame(update);
      }
    },
    { capture: true, passive: true },
  );
  addEventListener('scroll', () => target && place(), { capture: true, passive: true });
  addEventListener('resize', () => target && place(), { passive: true });
  document.addEventListener('fullscreenchange', () => target && place());

  refresh();
})();
