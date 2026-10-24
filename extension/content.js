// Alt+click on a link leaves that download to the browser, as in IDM.
document.addEventListener(
  'click',
  (e) => {
    if (!e.altKey) return;
    const link = e.target.closest?.('a[href]');
    if (!link) return;
    const api = globalThis.browser ?? globalThis.chrome;
    api.runtime.sendMessage({ type: 'bypass', url: link.href })?.catch?.(() => {});
  },
  true,
);
