// Drives the built board in headless Chrome, against a real `rodu` binary: the checks that unit
// and HTTP tests cannot make, such as what the page renders and how it reacts to the server.
//
//   node xtask/browser/smoke.mjs <path to rodu>      (or: cargo xtask browser-smoke [rodu])
//
// Needs Node 22 or later (built-in fetch and WebSocket) and Chrome; CHROME_PATH picks another
// Chromium browser. It talks to Chrome over the DevTools protocol, so it adds no dependency.
// Every scenario runs in its own temporary folders, with a stand-in Google Drive home, and the
// binary must carry the board (as `cargo xtask dist` builds it).

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, cpSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

// Absolute: each rodu runs in its own folder.
const binary = process.argv[2] && resolve(process.argv[2]);
if (!binary || !existsSync(binary)) {
  console.error('usage: node xtask/browser/smoke.mjs <path to rodu>');
  process.exit(2);
}
const root = mkdtempSync(join(tmpdir(), 'rodu-browser-'));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const results = [];
const check = (name, ok, detail = '') => {
  results.push(ok);
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${ok || !detail ? '' : `: ${detail}`}`);
};

// --- rodu -------------------------------------------------------------------------------------

/// Environment for one person: their home (where Google Drive is looked for) and system name.
function person(name) {
  const home = join(root, `home-${name}`);
  mkdirSync(home, { recursive: true });
  const env = { ...process.env, HOME: home, USERPROFILE: home, USER: name, USERNAME: name };
  delete env.RODU_DIR;
  delete env.RODU_WEB_DIST;
  return { name, home, env };
}

/// A stand-in My Drive, laid out as Google Drive for desktop does on macOS.
function myDrive(who, account) {
  const path = join(who.home, 'Library', 'CloudStorage', `GoogleDrive-${account}`, 'My Drive');
  mkdirSync(path, { recursive: true });
  return path;
}

function rodu(who, cwd, args) {
  const run = spawnSync(binary, args, { cwd, env: who.env, encoding: 'utf8' });
  if (run.status !== 0) throw new Error(`rodu ${args.join(' ')}: ${run.error ?? run.stderr}`);
  return run.stdout;
}

/// `rodu web` in `cwd`, once it printed its link.
async function web(who, cwd) {
  const child = spawn(binary, ['web', '--port', '0', '--no-open'], { cwd, env: who.env });
  const printed = [];
  let errors = '';
  child.stderr.on('data', (d) => (errors += d));
  child.stdout.on('data', (d) => printed.push(...String(d).split('\n').filter(Boolean)));
  for (let i = 0; i < 300; i++) {
    const line = printed.find((l) => l.startsWith('Open: '));
    if (line) {
      const link = line.slice('Open: '.length).trim();
      const base = link.split('#')[0].replace(/\/$/, '');
      const token = link.split('#token=')[1];
      const api = async (method, path, body) => {
        const reply = await fetch(base + path, {
          method,
          headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
          body: body === undefined ? undefined : JSON.stringify(body),
        });
        return { status: reply.status, text: await reply.text() };
      };
      return { child, link, api, printed, errors: () => errors };
    }
    if (child.exitCode !== null) break;
    await sleep(100);
  }
  child.kill();
  throw new Error(`rodu web printed no link: ${errors}`);
}

// --- Chrome -----------------------------------------------------------------------------------

function chromePath() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  const known = {
    darwin: ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'],
    win32: [
      `${process.env.PROGRAMFILES}\\Google\\Chrome\\Application\\chrome.exe`,
      `${process.env['PROGRAMFILES(X86)']}\\Google\\Chrome\\Application\\chrome.exe`,
      `${process.env.LOCALAPPDATA}\\Google\\Chrome\\Application\\chrome.exe`,
    ],
    linux: ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser'],
  };
  const found = (known[process.platform] ?? []).find((p) => existsSync(p));
  if (!found) throw new Error('Chrome not found: set CHROME_PATH');
  return found;
}

async function chrome() {
  const profile = join(root, 'chrome');
  const child = spawn(chromePath(), [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    '--no-first-run', '--no-default-browser-check', 'about:blank',
  ], { stdio: 'ignore' });
  const portFile = join(profile, 'DevToolsActivePort');
  for (let i = 0; i < 200 && !existsSync(portFile); i++) await sleep(100);
  const port = readFileSync(portFile, 'utf8').split('\n')[0];
  const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const socket = new WebSocket(targets.find((t) => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let id = 0;
  const pending = new Map();
  const page = { child, problems: [] };
  page.send = (method, params = {}) =>
    new Promise((resolve) => {
      pending.set(++id, resolve);
      socket.send(JSON.stringify({ id, method, params }));
    });
  socket.onmessage = ({ data }) => {
    const m = JSON.parse(data);
    if (m.id && pending.has(m.id)) {
      pending.get(m.id)(m);
      pending.delete(m.id);
    } else if (m.method === 'Page.javascriptDialogOpening') {
      // The board asks before sharing or removing someone: the smoke says yes.
      page.send('Page.handleJavaScriptDialog', { accept: true });
    } else if (m.method === 'Runtime.exceptionThrown') {
      page.problems.push(`exception: ${m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text}`);
    } else if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error') {
      page.problems.push(`${m.params.entry.text} ${m.params.entry.url ?? ''}`.trim());
    } else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') {
      page.problems.push(`console: ${m.params.args.map((a) => a.value ?? a.description).join(' ')}`);
    }
  };
  await page.send('Runtime.enable');
  await page.send('Log.enable');
  await page.send('Page.enable');
  await page.send('Emulation.setDeviceMetricsOverride', { width: 1200, height: 800, deviceScaleFactor: 1, mobile: false });
  page.eval = async (expression) => {
    const reply = await page.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (reply.result?.exceptionDetails) throw new Error(`${expression}: ${reply.result.exceptionDetails.exception?.description}`);
    return reply.result?.result?.value;
  };
  page.text = () => page.eval('document.body ? document.body.innerText : ""');
  page.until = async (what, test, ms = 30000) => {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      if (await test()) return;
      await sleep(200);
    }
    throw new Error(`timed out waiting for ${what}; page says: ${(await page.text()).slice(0, 500)}`);
  };
  page.untilText = (text, ms) => page.until(JSON.stringify(text), async () => (await page.text()).includes(text), ms);
  page.open = async (url) => {
    page.problems = [];
    await page.send('Page.navigate', { url });
  };
  // Sets a field's value as typing does, so the page's signals see it.
  page.fill = (selector, value) =>
    page.eval(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)});
      if (!el) throw new Error('no ' + ${JSON.stringify(selector)});
      el.value = ${JSON.stringify(value)};
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()`);
  page.click = (selector, text) =>
    page.eval(`(() => {
      const all = [...document.querySelectorAll(${JSON.stringify(selector)})];
      const el = all.find((e) => ${JSON.stringify(text ?? '')} === '' || e.textContent.includes(${JSON.stringify(text ?? '')}));
      if (!el) throw new Error('nothing to click: ' + ${JSON.stringify(`${selector} ${text ?? ''}`)});
      el.click();
      return true;
    })()`);
  return page;
}

/// The page's errors, but those a scenario expects (a refused request, the server restarting).
function unexpected(page, expected) {
  return page.problems.filter((p) => !expected.some((e) => e.test(p)));
}

// --- scenarios --------------------------------------------------------------------------------

async function board(page) {
  const ann = person('ann');
  const dir = join(root, 'board');
  mkdirSync(dir);
  rodu(ann, dir, ['init', '--name', 'ann', '--key', 'DEMO', '--title', 'Demo']);
  rodu(ann, dir, ['add', 'First card']);
  const server = await web(ann, dir);
  try {
    // The description comes from anyone on the team: nothing in it may run or load.
    const body = [
      '## Plan', '', '- one', '- two', '',
      '[site](https://example.com/a) [bad](javascript:window.__smoke=1)', '',
      '<img src=x onerror="window.__smoke=2"> <script>window.__smoke=3</script>', '',
      '![picture](https://example.com/p.png)',
    ].join('\n');
    const patched = await server.api('PATCH', '/api/items/DEMO-1', { patch: { body } });
    if (patched.status !== 200) throw new Error(`PATCH: ${patched.text}`);

    await page.open(server.link);
    await page.untilText('First card');
    check('board: the board shows the card', true);
    check('board: the token leaves the address bar', !(await page.eval('location.href')).includes('token='));

    await page.click('.card', 'First card');
    await page.until('the card panel', () => page.eval(`!!document.querySelector('aside.panel .description .markdown h2')`));
    const rendered = await page.eval(`(() => {
      const md = document.querySelector('aside.panel .description .markdown');
      const links = [...md.querySelectorAll('a')];
      return {
        heading: md.querySelector('h2')?.textContent,
        items: md.querySelectorAll('li').length,
        safe: links.filter((a) => a.href.startsWith('https://example.com/a')).every((a) => a.rel.includes('noopener') && a.target === '_blank'),
        scripted: links.some((a) => a.getAttribute('href')?.startsWith('javascript')),
        images: md.querySelectorAll('img, script').length,
        text: md.textContent,
      };
    })()`);
    await sleep(300);
    check('board: card markdown renders headings and lists', rendered.heading === 'Plan' && rendered.items === 2, JSON.stringify(rendered));
    check('board: links open safely in a new tab', rendered.safe && !rendered.scripted, JSON.stringify(rendered));
    check('board: raw HTML and images stay inert', rendered.images === 0 && rendered.text.includes('<script>'), JSON.stringify(rendered));
    check('board: nothing in the card ran', (await page.eval('window.__smoke')) === undefined);
    await page.click('aside.panel [aria-label="Close"]');

    // A card made on the board reaches the workspace.
    await page.click('.column-head .add');
    await page.fill('[aria-label="New item title"]', 'Made in the browser');
    await page.eval(`document.querySelector('form.new-card').requestSubmit()`);
    await page.untilText('Made in the browser');
    const listed = rodu(ann, dir, ['ls']);
    check('board: a card made on the board is in the workspace', listed.includes('Made in the browser'), listed);

    // A collection made elsewhere joins the list on its own; one made on the board is chosen.
    rodu(ann, dir, ['collection', 'add', 'OPS', '--title', 'Operations']);
    await page.until('OPS in the list', () => page.eval(`[...document.querySelectorAll('select[aria-label="Collection"] option')].some((o) => o.value === 'OPS')`));
    check('board: a collection made in the CLI appears without a reload', true);
    await page.click('.add-collection');
    await page.fill('[aria-label="Collection key"]', 'WEB');
    await page.fill('[aria-label="Collection name"]', 'From the web');
    await page.eval(`document.querySelector('form.new-collection').requestSubmit()`);
    await page.until('WEB chosen', () => page.eval(`document.querySelector('select[aria-label="Collection"]').value === 'WEB'`));
    check('board: a collection made on the board is chosen', !(await page.text()).includes('First card'));
    check('board: the collection is in the workspace', rodu(ann, dir, ['collections']).includes('WEB'));

    await page.send('Emulation.setDeviceMetricsOverride', { width: 375, height: 760, deviceScaleFactor: 1, mobile: true });
    await sleep(300);
    check('board: no sideways scroll at phone width', await page.eval('document.documentElement.scrollWidth <= 375'));
    await page.send('Emulation.setDeviceMetricsOverride', { width: 1200, height: 800, deviceScaleFactor: 1, mobile: false });
    const left = unexpected(page, []);
    check('board: no page errors', left.length === 0, left.join(' | '));
  } finally {
    server.child.kill();
  }
}

async function share(page) {
  const ann = person('sharer');
  const drive = myDrive(ann, 'sharer@example.com');
  const dir = join(root, 'share');
  mkdirSync(dir);
  rodu(ann, dir, ['init', '--name', 'sharer', '--key', 'SHR', '--title', 'Shared']);
  rodu(ann, dir, ['add', 'Made before sharing']);
  const server = await web(ann, dir);
  try {
    await page.open(server.link);
    await page.untilText('Made before sharing');
    await page.click('.team-button');
    await page.untilText('Share with a team');
    check('share: the Team panel offers this machine\'s Google Drive', (await page.text()).includes('sharer@example.com'));
    // Marks this page, so the reload onto the team's board is seen.
    await page.eval('window.__before = true');
    await page.click('.team-share button', 'Share this board');
    // The page reloads onto the team's board, on the same link.
    await page.until('the board reopened', async () => server.printed.some((l) => l.startsWith('Reopened the board')));
    await page.until('the team board', async () => (await server.api('GET', '/api/team')).text.includes('"signed":true'));
    check('share: the folder holds the team', existsSync(join(drive, 'Rodu', 'SHR', 'rodu-team.json')));
    await page.until('the page reloaded', async () => (await page.eval('window.__before')) === undefined);
    await page.untilText('Made before sharing');
    await page.click('.team-button');
    await page.untilText('Encrypted');
    check('share: the board comes back as the team\'s', (await page.text()).includes('team owner'));
    const shown = `${server.printed.join('\n')}${server.errors()}${await page.eval('document.body.innerHTML')}`;
    check('share: no invite code on the page or in the terminal', !/rodu[12]-/.test(shown));
    // A plain board has no team yet (404), and the board restarts under the page.
    const left = unexpected(page, [/status of 404/, /ERR_CONNECTION_REFUSED/, /status of 503/]);
    check('share: no unexpected page errors', left.length === 0, left.join(' | '));
  } finally {
    server.child.kill();
  }
}

async function joinScenario(page) {
  const ann = person('owner');
  const annDrive = myDrive(ann, 'owner@example.com');
  const teamDir = join(root, 'team');
  mkdirSync(teamDir);
  rodu(ann, teamDir, ['team', 'create']);
  rodu(ann, teamDir, ['add', 'Card from the owner']);
  const code = rodu(ann, teamDir, ['team', '--show-invite'])
    .split('\n').find((l) => l.startsWith('Invite code: '))?.slice('Invite code: '.length).trim();
  if (!code) throw new Error('no invite code');
  const bob = person('bob');
  const bobDrive = myDrive(bob, 'bob@example.org');
  cpSync(join(annDrive, 'Rodu'), join(bobDrive, 'From owner', 'Rodu'), { recursive: true });
  const dir = join(root, 'bob');
  mkdirSync(dir);
  const server = await web(bob, dir);
  try {
    await page.open(server.link);
    await page.untilText('There is no board in this folder yet');
    check('join: a folder with no workspace offers to join', true);

    await page.fill('.join-form textarea', code);
    await page.click('.join-form input[type=checkbox]');
    await page.click('.join-form button[type=submit]');
    await page.untilText('Give your name on the team');
    check('join: already on the team with no name is refused', !existsSync(join(dir, '.rodu')));
    await page.click('.join-form input[type=checkbox]');

    await page.fill('.join-form textarea', 'rodu2-not-an-invite');
    await page.click('.join-form button[type=submit]');
    await page.until('the refusal', () => page.eval(`!!document.querySelector('.join-form [role=alert]')`));
    check('join: a wrong invite keeps the form, with the reason', !existsSync(join(dir, '.rodu')));

    await page.fill('.join-form textarea', code);
    await page.click('.join-form button[type=submit]');
    await page.untilText('Joined the team');
    const said = await page.text();
    check('join: the machine\'s code stays on screen', said.includes("This machine's code"), said);
    await page.untilText('Open the board');
    const html = await page.eval('document.body.innerHTML');
    check('join: the invite is gone from the page', !html.includes(code));
    await page.click('button', 'Open the board');
    await page.untilText('Card from the owner');
    check('join: the team\'s board opens on the same link', true);
    const terminal = `${server.printed.join('\n')}${server.errors()}`;
    check('join: the invite never reaches the terminal', !terminal.includes(code));
    // The no-workspace probe (404), the refused invite (400), and the restart under the page.
    const left = unexpected(page, [/status of (400|404|503)/, /ERR_CONNECTION_REFUSED/]);
    check('join: no unexpected page errors', left.length === 0, left.join(' | '));
  } finally {
    server.child.kill();
  }
}

// --- run --------------------------------------------------------------------------------------

let page;
try {
  page = await chrome();
  for (const [name, scenario] of [['board', board], ['share', share], ['join', joinScenario]]) {
    try {
      await scenario(page);
    } catch (e) {
      check(`${name}: scenario finished`, false, e.message.slice(0, 800));
    }
  }
} finally {
  page?.child.kill();
  await sleep(500);
  try {
    rmSync(root, { recursive: true, force: true, maxRetries: 5 });
  } catch {
    // A file Chrome still holds on Windows; the temporary folder goes in time.
  }
}
const failed = results.filter((ok) => !ok).length;
console.log(`\n${results.length - failed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
