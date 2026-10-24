const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);

function showStatus(text, kind, detail = '') {
  $('status').textContent = text;
  $('status').className = `status ${kind}`;
  $('detail').textContent = detail;
  $('detail').hidden = !detail;
}

/** Turns the browser's native messaging errors into what to do about them. */
function explain(error) {
  if (/not found|no such native application/i.test(error)) {
    return "Slipstream isn't set up for this browser yet. Extract the download to a folder, keep slipstream-bridge next to slipstream-desktop, and open slipstream-desktop once.";
  }
  if (/forbidden/i.test(error)) {
    return "This copy of the extension isn't allowed to talk to Slipstream. Load the extension folder that came with Slipstream.";
  }
  if (/exited|communicating|disconnected/i.test(error)) {
    return 'Slipstream\'s bridge was found but stopped. If you moved Slipstream, open it once from its new folder; security software may also be blocking slipstream-bridge.';
  }
  return "Can't reach Slipstream.";
}

async function checkStatus() {
  const result = await api.runtime.sendMessage({ type: 'status' });
  const answer = result?.answer;
  if (!result?.ok) {
    showStatus(explain(result?.error ?? ''), 'err', `Browser said: ${result?.error ?? 'no answer'}`);
  } else if (answer?.appRunning) {
    showStatus(`Connected to Slipstream ${answer.version}`, 'ok');
  } else {
    showStatus('Slipstream is installed and starts when needed', 'ok');
  }
  const { problem } = (await api.storage.session?.get('problem').catch(() => ({}))) ?? {};
  if (problem && !answer?.appRunning) showStatus(problem, 'warn');
}

/** Videos the current page has played, each with a Download button. */
async function showVideos() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  if (!tab) return;
  const answer = await api.runtime.sendMessage({ type: 'media-list', tabId: tab.id }).catch(() => null);
  const items = answer?.items ?? [];
  $('videos').hidden = !items.length;
  $('video-list').replaceChildren(
    ...items.map((entry) => {
      const li = document.createElement('li');
      const what = document.createElement('div');
      what.className = 'what';
      const title = document.createElement('b');
      title.textContent = entry.title;
      const detail = document.createElement('small');
      detail.textContent = entry.detail;
      detail.title = entry.url;
      what.append(title, detail);
      const button = document.createElement('button');
      button.textContent = 'Download';
      button.addEventListener('click', async () => {
        button.disabled = true;
        button.textContent = 'Sending…';
        const result = await api.runtime.sendMessage({ type: 'download-media', tabId: tab.id, url: entry.url });
        if (result?.ok) window.close();
        button.disabled = false;
        button.textContent = 'Download';
        checkStatus();
      });
      li.append(what, button);
      return li;
    }),
  );
}

async function init() {
  // The background script owns the defaults.
  const current = await api.runtime.sendMessage({ type: 'settings' });
  $('enabled').checked = current.enabled;
  $('video-button').checked = current.videoButton;
  $('video-button').addEventListener('change', (e) => api.storage.local.set({ videoButton: e.target.checked }));
  $('extensions').value = current.extensions;
  $('min-size').value = current.minSizeMb;

  $('enabled').addEventListener('change', (e) => api.storage.local.set({ enabled: e.target.checked }));
  $('extensions').addEventListener('change', (e) => api.storage.local.set({ extensions: e.target.value.trim() }));
  $('min-size').addEventListener('change', (e) => api.storage.local.set({ minSizeMb: Math.max(0, Number(e.target.value) || 0) }));
  $('open').addEventListener('click', () => {
    api.runtime.sendMessage({ type: 'activate' });
    window.close();
  });
  checkStatus();
  showVideos();
}

init();
