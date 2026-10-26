import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i], process.argv[i + 1]);
for (const key of ['--duckdb', '--extension', '--quack-extension']) assert(args.get(key), `missing ${key}`);
const fixture = mkdtempSync(join(tmpdir(), 'sqlnb-windows-runtime-'));
const assets = join(fixture, 'assets');
const notebooks = join(fixture, 'notebooks');
mkdirSync(assets);
mkdirSync(notebooks);
writeFileSync(join(assets, 'index.html'), '<!doctype html><title>Windows native bridge fixture</title>');
const init = join(fixture, 'init.sql');
writeFileSync(init, '');
const child = spawn(args.get('--duckdb'), ['-unsigned', '-init', init, '-json', ':memory:'], {
  env: { ...process.env, HOME: fixture, USERPROFILE: fixture, LOCALAPPDATA: join(fixture, 'AppData', 'Local') },
  stdio: ['pipe', 'pipe', 'pipe'],
});
let errors = '';
child.stderr.on('data', chunk => { errors += chunk.toString(); });
const lines = createInterface({ input: child.stdout });
let pending;
lines.on('line', line => {
  if (pending && line.trim()) {
    const { resolve, reject, timer } = pending;
    pending = undefined;
    clearTimeout(timer);
    try { resolve(JSON.parse(line)); } catch (error) { reject(error); }
  }
});
child.on('exit', code => {
  if (pending) { clearTimeout(pending.timer); pending.reject(new Error(`DuckDB exited ${code}: ${errors}`)); pending = undefined; }
});
const literal = text => `'${text.replaceAll("'", "''")}'`;
function query(sql) {
  assert(!pending, 'queries must be serialized');
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending = undefined; reject(new Error(`SQL timed out: ${errors}`)); }, 120_000);
    pending = { resolve, reject, timer };
    child.stdin.write(`${sql};\n`);
  });
}
try {
  const rows = await query(`load ${literal(args.get('--quack-extension'))}; load ${literal(args.get('--extension'))}; select sqlnb_start(assets := ${literal(assets)}, notebook_root := ${literal(notebooks)}, open_browser := false) as status`);
  const state = JSON.parse(rows[0].status);
  const token = new URL(state.launchUrl).hash.match(/^#sidequery_token=([0-9a-f]{64})$/)?.[1];
  assert(token);
  const headers = { authorization: `Bearer ${token}`, origin: state.baseUrl, 'content-type': 'application/json' };
  async function request(path, method = 'GET', body, status = 200) {
    const response = await fetch(`${state.baseUrl}/api/v1/${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
    const result = await response.json();
    assert.equal(response.status, status, JSON.stringify(result));
    return result;
  }
  assert.equal((await request('bootstrap')).protocol, 'quack');
  assert.equal((await fetch(`${state.baseUrl}/api/v1/files`)).status, 401);
  const created = await request('file', 'PUT', { path: 'Unicode/π.sql', source: 'select 1;', expectedRevision: '' });
  const opened = await request(`file?${new URLSearchParams({ path: 'Unicode/π.sql' })}`);
  assert.equal(opened.source, 'select 1;');
  const saved = await request('file', 'PUT', { path: opened.path, source: 'select 2;', expectedRevision: created.revision });
  await request('file', 'PUT', { path: opened.path, source: 'stale', expectedRevision: created.revision }, 409);
  assert.equal(readFileSync(join(notebooks, 'Unicode', 'π.sql'), 'utf8'), 'select 2;');
  const copy = await request('file/duplicate', 'POST', { path: opened.path, destinationPath: 'copy.sql', expectedRevision: saved.revision });
  await request('file/move', 'POST', { path: 'copy.sql', destinationPath: 'moved.sql', expectedRevision: copy.revision });
  await request('file', 'DELETE', { path: 'moved.sql', expectedRevision: copy.revision });
  assert.deepEqual((await request('files')).files, [{ path: opened.path }]);
  assert((await request(`history?${new URLSearchParams({ path: opened.path })}`)).revisions.length >= 2);
  for (const path of ['CON.sql', 'file.sql:stream', '../escape.sql', 'folder./escape.sql']) {
    await request('file', 'PUT', { path, source: 'forbidden', expectedRevision: '' }, 400);
  }
  await query('select sqlnb_stop() as stopped');
  console.log('Windows native startup, authenticated HTTP, Unicode files, revisions, lifecycle, history, and path security passed');
} finally {
  child.stdin.end();
  child.kill();
  lines.close();
}
