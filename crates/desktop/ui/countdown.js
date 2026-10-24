// The last chance to cancel before the computer sleeps or turns off when
// the download queue is done.

import { $, call, decorateIcons, params, thisWindow } from './common.js';

const action = params.get('action') === 'sleep' ? 'sleep' : 'shutdown';
const SECONDS = action === 'sleep' ? 30 : 60;
const doing = action === 'sleep' ? 'Putting the computer to sleep' : 'Turning off the computer';
let left = SECONDS;
let timer = 0;

function show() {
  $('message').textContent = `${doing} in ${left} second${left === 1 ? '' : 's'}.`;
  $('bar').querySelector('i').style.width = `${((SECONDS - left) / SECONDS) * 100}%`;
}

async function go() {
  clearInterval(timer);
  $('message').textContent = `${doing}…`;
  await call('power_off', { whenDone: action });
  thisWindow().close();
}

decorateIcons();
$('now').textContent = action === 'sleep' ? 'Sleep now' : 'Turn off now';
$('now').addEventListener('click', go);
$('cancel').addEventListener('click', () => thisWindow().close());
addEventListener('keydown', (e) => e.key === 'Escape' && thisWindow().close());
show();
timer = setInterval(() => {
  left -= 1;
  show();
  if (left <= 0) go();
}, 1000);
$('cancel').focus();
