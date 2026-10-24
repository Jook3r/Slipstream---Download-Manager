const api = globalThis.browser ?? globalThis.chrome;
const $ = (id) => document.getElementById(id);

function showStatus(text, kind) {
  $('status').textContent = text;
  $('status').className = `status ${kind}`;
}

async function checkStatus() {
  const result = await api.runtime.sendMessage({ type: 'status' });
  const answer = result?.answer;
  if (!result?.ok) {
    showStatus("Can't reach Slipstream. Install the app and open it once.", 'err');
  } else if (answer?.appRunning) {
    showStatus(`Connected to Slipstream ${answer.version}`, 'ok');
  } else {
    showStatus('Slipstream is installed and starts when needed', 'ok');
  }
  const { problem } = (await api.storage.session?.get('problem').catch(() => ({}))) ?? {};
  if (problem && !answer?.appRunning) showStatus(problem, 'warn');
}

async function init() {
  // The background script owns the defaults.
  const current = await api.runtime.sendMessage({ type: 'settings' });
  $('enabled').checked = current.enabled;
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
}

init();
