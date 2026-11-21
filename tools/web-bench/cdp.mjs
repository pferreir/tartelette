// Drive a headless Chromium-family browser over the DevTools protocol, with
// nothing but Node 22+ (its built-in WebSocket). Used by scripts/web-bench.sh
// and scripts/web-check.sh.
//
//   node cdp.mjs URL DONE [--drop FILE] [--wait MS] [--click X,Y]... [--screenshot FILE]
//
// Opens URL and waits until the page has loaded and the JS expression DONE
// is true; then drops FILE on the middle of the page (as a user would) and
// waits MS (default 8000), clicks at each X,Y, and saves a screenshot. Page
// console output and errors are printed as they come; the last line is
// `RESULT <json>` with `window.results` (or the page title). Exits 1 if the
// page threw an exception or DONE never came true.
//
// The browser is $CHROME, else the first of thorium-browser, chromium,
// google-chrome-stable and google-chrome found on the PATH.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';

const args = process.argv.slice(2);
const [url, done] = args;
const opts = { click: [] };
for (let i = 2; i < args.length; i += 2) {
  const [flag, value] = [args[i], args[i + 1]];
  if (flag === '--click') opts.click.push(value.split(',').map(Number));
  else opts[flag.replace(/^--/, '')] = value;
}
if (!url || !done) {
  console.error('usage: node cdp.mjs URL DONE [--drop FILE] [--wait MS] [--click X,Y]... [--screenshot FILE]');
  process.exit(2);
}

const browserBin =
  process.env.CHROME ??
  ['thorium-browser', 'chromium', 'google-chrome-stable', 'google-chrome'].find(
    (b) => spawnSync('sh', ['-c', `command -v ${b}`]).status === 0,
  );
if (!browserBin) {
  console.error('cdp: no Chromium-family browser found (set CHROME)');
  process.exit(2);
}
const port = 9300 + Math.floor(Math.random() * 600);
const profile = mkdtempSync(tmpdir() + '/cdp-');
const browser = spawn(
  browserBin,
  [
    '--headless=new',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--window-size=1280,900',
    // WebGL without a GPU, for the front panel.
    '--enable-unsafe-swiftshader',
    'about:blank',
  ],
  { stdio: 'ignore' },
);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = false;
const exited = new Promise((r) => browser.on('exit', r));
const finish = async (code) => {
  // The browser writes to its profile until it is gone.
  browser.kill();
  await exited;
  rmSync(profile, { recursive: true, force: true, maxRetries: 5 });
  process.exit(code);
};

let targets;
for (let i = 0; i < 100 && !targets; i++) {
  await sleep(100);
  try {
    targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  } catch {}
}
if (!targets) {
  console.error(`cdp: ${browserBin} did not start`);
  await finish(2);
}
const ws = new WebSocket(targets.find((t) => t.type === 'page').webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener('open', r));
let next = 0;
const pending = new Map();
ws.addEventListener('message', (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  } else if (msg.method === 'Runtime.consoleAPICalled') {
    const text = msg.params.args.map((a) => a.value ?? a.description).join(' ');
    console.log(`console.${msg.params.type}: ${text}`);
  } else if (msg.method === 'Runtime.exceptionThrown') {
    const d = msg.params.exceptionDetails;
    console.log(`EXCEPTION: ${d.exception?.description ?? d.text}`);
    failed = true;
  } else if (msg.method === 'Log.entryAdded') {
    console.log(`log.${msg.params.entry.level}: ${msg.params.entry.text}`);
  }
});
const send = (method, params = {}) =>
  new Promise((r) => {
    const id = ++next;
    pending.set(id, r);
    ws.send(JSON.stringify({ id, method, params }));
  });
const evaluate = async (expression) =>
  (await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })).result
    ?.result?.value;

await send('Runtime.enable');
await send('Log.enable');
await send('Page.enable');
await send('Page.navigate', { url });
const ready = `location.href === ${JSON.stringify(url)} && document.readyState === 'complete' && (${done})`;
const start = Date.now();
while (!(await evaluate(ready))) {
  if (Date.now() - start > Number(opts.timeout ?? 600000)) {
    console.error(`cdp: timed out waiting for ${done}`);
    await finish(1);
  }
  await sleep(250);
}
if (opts.drop) {
  const at = { x: 640, y: 450 };
  const data = { items: [], files: [opts.drop], dragOperationsMask: 1 };
  for (const type of ['dragEnter', 'dragOver', 'drop']) {
    await send('Input.dispatchDragEvent', { type, ...at, data });
  }
  await sleep(Number(opts.wait ?? 8000));
}
for (const [x, y] of opts.click) {
  for (const type of ['mousePressed', 'mouseReleased']) {
    await send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 });
  }
  await sleep(1500);
}
console.log('RESULT', JSON.stringify(await evaluate('window.results ?? document.title')));
if (opts.screenshot) {
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  writeFileSync(opts.screenshot, Buffer.from(shot.result.data, 'base64'));
}
ws.close();
await finish(failed ? 1 : 0);
