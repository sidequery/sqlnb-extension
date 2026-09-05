import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const values = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  values.set(process.argv[index], process.argv[index + 1]);
}
const duckdb = values.get('--duckdb');
const extension = values.get('--extension');
const quackExtension = values.get('--quack-extension');
const httpfsExtension = values.get('--httpfs-extension');
assert(
  duckdb && extension && quackExtension && httpfsExtension,
  'usage: runtime-smoke.mjs --duckdb PATH --extension PATH --quack-extension PATH --httpfs-extension PATH',
);
assert(statSync(extension).size > 0, `empty Sidequery UI extension: ${extension}`);
assert(statSync(quackExtension).size > 0, `empty Quack extension: ${quackExtension}`);
assert(statSync(httpfsExtension).size > 0, `empty httpfs extension: ${httpfsExtension}`);

function sqlLiteral(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

const fixture = mkdtempSync(join(tmpdir(), 'sidequery-ui-assets.'));
writeFileSync(join(fixture, 'index.html'), '<!doctype html><title>Sidequery local fixture</title>');
const notebookRoot = mkdtempSync(join(tmpdir(), 'sidequery-ui-notebooks.'));
const outsideRoot = mkdtempSync(join(tmpdir(), 'sidequery-ui-outside.'));
mkdirSync(join(notebookRoot, 'nested'));
writeFileSync(join(notebookRoot, 'first.sqlnb'), '-- first notebook\nselect 1;\n');
writeFileSync(join(notebookRoot, 'nested', 'query.sql'), 'select 2;\n');
writeFileSync(join(notebookRoot, 'ignored.txt'), 'not a notebook');
symlinkSync(outsideRoot, join(notebookRoot, 'outside-link'));

const child = spawn(duckdb, [':memory:', '-json'], { stdio: ['pipe', 'pipe', 'pipe'] });
let stdout = '';
let stderr = '';
const waiters = [];
child.stderr.on('data', (chunk) => {
  stderr += chunk;
});
child.stdout.on('data', (chunk) => {
  stdout += chunk;
  while (stdout.includes('\n') && waiters.length) {
    const newline = stdout.indexOf('\n');
    const line = stdout.slice(0, newline).trim();
    stdout = stdout.slice(newline + 1);
    if (line) {
      waiters.shift()(line);
    }
  }
});

function runSQL(sql) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error(`DuckDB output timeout for ${sql}\n${stderr}`)),
      20_000,
    );
    waiters.push((line) => {
      clearTimeout(timeout);
      try {
        resolve(JSON.parse(line));
      } catch (error) {
        reject(new Error(`${error.message}: ${line}\n${stderr}`));
      }
    });
    child.stdin.write(`${sql};\n`);
  });
}

function firstValue(rows) {
  return Object.values(rows[0])[0];
}

assert.equal(
  firstValue(
    await runSQL(
      `load ${sqlLiteral(httpfsExtension)}; load ${sqlLiteral(quackExtension)}; load ${sqlLiteral(extension)}; select true as loaded`,
    ),
  ),
  true,
);
assert.equal(
  firstValue(await runSQL("select sidequery_ui_start(null, '', 0, false) is null as propagated")),
  true,
);
assert.equal(firstValue(await runSQL('select count(*) as count from quack_server_list()')), 0);
assert.equal(
  firstValue(await runSQL('create table shared_table as select 7::integer as value; select true')),
  true,
);
assert.equal(
  firstValue(await runSQL('create temp table caller_temp as select 9::integer as value; select true')),
  true,
);
const startRows = await runSQL(
  `select sidequery_ui_start('', ${sqlLiteral(fixture)}, 0, false, ${sqlLiteral(notebookRoot)}) as status`,
);
const start = JSON.parse(firstValue(startRows));
assert.equal(start.engine, 'native-duckdb');
assert.equal(start.protocol, 'quack');
const launch = new URL(start.launchUrl);
const token = launch.hash.match(/^#sidequery_token=([0-9a-f]{64})$/)?.[1];
assert(token, `missing launch capability: ${start.launchUrl}`);
assert.equal(
  firstValue(
    await runSQL(
      'select count(*) as count from (select sidequery_ui_status() as status from range(3)) where status is not null',
    ),
  ),
  3,
);
const headers = { authorization: `Bearer ${token}` };

const rootResponse = await fetch(`${start.baseUrl}/`);
assert.equal(rootResponse.status, 200);
assert.match(await rootResponse.text(), /Sidequery local fixture/);
assert.match(rootResponse.headers.get('content-security-policy') ?? '', /connect-src 'self'/);
assert.equal(rootResponse.headers.get('cache-control'), 'no-store');

assert.equal((await fetch(`${start.baseUrl}/api/v1/bootstrap`)).status, 401);
assert.equal(
  (
    await fetch(`${start.baseUrl}/api/v1/bootstrap`, {
      headers: { ...headers, origin: 'https://attacker.example' },
    })
  ).status,
  401,
);
const bootstrapResponse = await fetch(`${start.baseUrl}/api/v1/bootstrap`, { headers });
assert.equal(bootstrapResponse.status, 200);
const bootstrap = await bootstrapResponse.json();
assert.deepEqual(
  {
    protocol: bootstrap.protocol,
    endpoint: bootstrap.endpoint,
    token: bootstrap.token,
    supportsCancel: bootstrap.supportsCancel,
  },
  { protocol: 'quack', endpoint: '/quack', token, supportsCancel: false },
);

assert.equal((await fetch(`${start.baseUrl}/api/v1/files`)).status, 401);
const filesResponse = await fetch(`${start.baseUrl}/api/v1/files`, { headers });
assert.equal(filesResponse.status, 200);
const files = await filesResponse.json();
assert.equal(files.root, realpathSync(notebookRoot));
assert.deepEqual(files.files, [{ path: 'first.sqlnb' }, { path: 'nested/query.sql' }]);

const firstResponse = await fetch(
  `${start.baseUrl}/api/v1/file?${new URLSearchParams({ path: 'first.sqlnb' })}`,
  { headers },
);
assert.equal(firstResponse.status, 200);
const first = await firstResponse.json();
assert.equal(first.path, 'first.sqlnb');
assert.equal(first.source, '-- first notebook\nselect 1;\n');
assert.match(first.revision, /^[0-9a-f]{64}$/);

const updatedSource = '-- updated notebook\nselect 42;\n';
const updateResponse = await fetch(`${start.baseUrl}/api/v1/file`, {
  method: 'PUT',
  headers: { ...headers, 'content-type': 'application/json' },
  body: JSON.stringify({
    path: 'first.sqlnb',
    source: updatedSource,
    expectedRevision: first.revision,
  }),
});
assert.equal(updateResponse.status, 200);
const updated = await updateResponse.json();
assert.match(updated.revision, /^[0-9a-f]{64}$/);
assert.notEqual(updated.revision, first.revision);
assert.equal(readFileSync(join(notebookRoot, 'first.sqlnb'), 'utf8'), updatedSource);

const conflictResponse = await fetch(`${start.baseUrl}/api/v1/file`, {
  method: 'PUT',
  headers: { ...headers, 'content-type': 'application/json' },
  body: JSON.stringify({
    path: 'first.sqlnb',
    source: 'must not replace the file',
    expectedRevision: first.revision,
  }),
});
assert.equal(conflictResponse.status, 409);
assert.equal((await conflictResponse.json()).error.code, 'revision_conflict');
assert.equal(readFileSync(join(notebookRoot, 'first.sqlnb'), 'utf8'), updatedSource);

const createResponse = await fetch(`${start.baseUrl}/api/v1/file`, {
  method: 'PUT',
  headers: { ...headers, 'content-type': 'application/json' },
  body: JSON.stringify({
    path: 'new/deep/notebook.sqlnb',
    source: 'select 99;\n',
    expectedRevision: '',
  }),
});
assert.equal(createResponse.status, 200);
assert.equal(readFileSync(join(notebookRoot, 'new', 'deep', 'notebook.sqlnb'), 'utf8'), 'select 99;\n');
const createConflictResponse = await fetch(`${start.baseUrl}/api/v1/file`, {
  method: 'PUT',
  headers: { ...headers, 'content-type': 'application/json' },
  body: JSON.stringify({
    path: 'new/deep/notebook.sqlnb',
    source: 'select 100;\n',
    expectedRevision: '',
  }),
});
assert.equal(createConflictResponse.status, 409);

for (const path of ['../outside.sqlnb', '/tmp/outside.sqlnb', 'outside-link/escaped.sqlnb']) {
  const response = await fetch(
    `${start.baseUrl}/api/v1/file?${new URLSearchParams({ path })}`,
    { headers },
  );
  assert(response.status >= 400, `unsafe notebook path was accepted: ${path}`);
}
const symlinkWriteResponse = await fetch(`${start.baseUrl}/api/v1/file`, {
  method: 'PUT',
  headers: { ...headers, 'content-type': 'application/json' },
  body: JSON.stringify({
    path: 'outside-link/escaped.sqlnb',
    source: 'select secret;\n',
    expectedRevision: '',
  }),
});
assert(symlinkWriteResponse.status >= 400);
assert.equal(existsSync(join(outsideRoot, 'escaped.sqlnb')), false);
assert.equal(readFileSync(join(notebookRoot, 'first.sqlnb'), 'utf8'), updatedSource);

const rejectedProxy = await fetch(`${start.baseUrl}/quack`, {
  method: 'POST',
  headers: { 'content-type': 'application/vnd.duckdb', origin: 'https://attacker.example' },
  body: new Uint8Array([0]),
});
assert.equal(rejectedProxy.status, 401);

const endpoint = new URL(bootstrap.endpoint, start.baseUrl);
const quackListenUrl = firstValue(await runSQL('select listen_url from quack_server_list()'));
const directEndpoint = new URL(quackListenUrl);
const directClientSQL = [
  `load ${sqlLiteral(quackExtension)}`,
  `attach ${sqlLiteral(`quack:${directEndpoint.hostname}:${directEndpoint.port}`)} as sidequery_direct (token ${sqlLiteral(token)})`,
  'select value from sidequery_direct.main.shared_table',
].join('; ');
const directClient = spawnSync(duckdb, [':memory:', '-csv', '-noheader', '-c', directClientSQL], {
  encoding: 'utf8',
  timeout: 20_000,
});
assert.equal(directClient.status, 0, directClient.stderr || directClient.stdout);
assert.equal(directClient.stdout.trim(), '7');

const clientSQL = [
  `load ${sqlLiteral(quackExtension)}`,
  `attach ${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)} as sidequery_native (token ${sqlLiteral(token)})`,
  'select value from sidequery_native.main.shared_table',
].join('; ');
const client = spawnSync(duckdb, [':memory:', '-csv', '-noheader', '-c', clientSQL], {
  encoding: 'utf8',
  timeout: 20_000,
});
assert.equal(client.status, 0, client.stderr || client.stdout);
assert.equal(client.stdout.trim(), '7');

const missingTemp = spawnSync(
  duckdb,
  [
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      `load ${sqlLiteral(quackExtension)}`,
      `from quack_query(${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)}, ${sqlLiteral('select * from caller_temp')}, token = ${sqlLiteral(token)})`,
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: 20_000 },
);
assert.notEqual(missingTemp.status, 0, 'Quack session must not see the caller temporary table');

const remoteStop = spawnSync(
  duckdb,
  [
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      `load ${sqlLiteral(quackExtension)}`,
      `from quack_query(${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)}, ${sqlLiteral('select sidequery_ui_stop()')}, token = ${sqlLiteral(token)})`,
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: 20_000 },
);
assert.notEqual(remoteStop.status, 0, 'a Quack client must not stop its own server');
assert.match(
  `${remoteStop.stderr}\n${remoteStop.stdout}`,
  /only be called from the connection that started the Sidequery UI/,
);

const serversBeforeStop = await runSQL('select count(*) as count from quack_server_list()');
assert.equal(firstValue(serversBeforeStop), 1);
assert.equal(firstValue(await runSQL('select sidequery_ui_stop()')), true);
assert.equal(firstValue(await runSQL('select count(*) as count from quack_server_list()')), 0);

const upstreamPaths = [];
let release = 'one';
const remoteFiles = () => ({
  'index.html': '<!doctype html><script src="asset.js"></script>',
  'asset.js': `globalThis.sidequeryRemoteFixture = '${release}';`,
});
const upstream = createServer((request, response) => {
  upstreamPaths.push(request.url);
  if (request.url === '/notebook/manifest.json') {
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify({
      version: release === 'one' ? '1111111' : '2222222',
      bridgeVersion: 1,
      files: Object.entries(remoteFiles()).sort(([a], [b]) => a.localeCompare(b)).map(([path, body]) => ({
        path, size: Buffer.byteLength(body), sha256: createHash('sha256').update(body).digest('hex'),
      })),
    }));
    return;
  }
  const asset = remoteFiles()[request.url?.replace('/notebook/', '')];
  if (asset !== undefined) {
    response.end(asset);
    return;
  }
  response.statusCode = 404;
  response.end('not found');
});
await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
const upstreamPort = upstream.address().port;
const remoteRows = await runSQL(
  `select sidequery_ui_start('http://127.0.0.1:${upstreamPort}/notebook/', '', 0, false, ${sqlLiteral(notebookRoot)}) as status`,
);
const remote = JSON.parse(firstValue(remoteRows));
const remoteAsset = await fetch(`${remote.baseUrl}/asset.js`);
assert.equal(remoteAsset.status, 200);
assert.match(await remoteAsset.text(), /sidequeryRemoteFixture = 'one'/);
assert.equal((await fetch(`${remote.baseUrl}/../outside`)).status, 404);
assert(upstreamPaths.includes('/notebook/manifest.json'));
assert(!upstreamPaths.includes('/notebook/outside'));
assert.equal(firstValue(await runSQL('select sidequery_ui_stop()')), true);
release = 'two';
const updatedUi = JSON.parse(firstValue(await runSQL(
  `select sidequery_ui_start('http://127.0.0.1:${upstreamPort}/notebook/', '', 0, false, ${sqlLiteral(notebookRoot)}) as status`,
)));
assert.match(await (await fetch(`${updatedUi.baseUrl}/asset.js`)).text(), /sidequeryRemoteFixture = 'two'/);
assert.equal(firstValue(await runSQL('select sidequery_ui_stop()')), true);
await new Promise((resolve) => upstream.close(resolve));
const offline = JSON.parse(firstValue(await runSQL(
  `select sidequery_ui_start('http://127.0.0.1:${upstreamPort}/notebook/', '', 0, false, ${sqlLiteral(notebookRoot)}) as status`,
)));
assert.match(await (await fetch(`${offline.baseUrl}/asset.js`)).text(), /sidequeryRemoteFixture = 'two'/);
assert.equal(firstValue(await runSQL('select sidequery_ui_stop()')), true);

const externalAccessDisabled = spawnSync(
  duckdb,
  [
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      `load ${sqlLiteral(httpfsExtension)}`,
      `load ${sqlLiteral(quackExtension)}`,
      `load ${sqlLiteral(extension)}`,
      'set enable_external_access = false',
      `select sidequery_ui_start('', ${sqlLiteral(fixture)}, 0, false, ${sqlLiteral(notebookRoot)})`,
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: 20_000 },
);
assert.notEqual(externalAccessDisabled.status, 0);
assert.match(
  `${externalAccessDisabled.stderr}\n${externalAccessDisabled.stdout}`,
  /requires enable_external_access=true/,
);

const defaultHome = mkdtempSync(join(tmpdir(), 'sidequery-ui-home.'));
const defaultRootStart = spawnSync(
  duckdb,
  [
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      `load ${sqlLiteral(httpfsExtension)}`,
      `load ${sqlLiteral(quackExtension)}`,
      `load ${sqlLiteral(extension)}`,
      `select sidequery_ui_start('', ${sqlLiteral(fixture)}, 0, false)`,
      'select sidequery_ui_stop()',
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: 20_000, env: { ...process.env, HOME: defaultHome } },
);
assert.equal(defaultRootStart.status, 0, defaultRootStart.stderr || defaultRootStart.stdout);
assert.equal(existsSync(join(defaultHome, 'Documents', 'sqlnb')), true);

child.stdin.end();
await new Promise((resolve, reject) => {
  child.on('exit', (code) =>
    code === 0 ? resolve() : reject(new Error(`DuckDB exited ${code}: ${stderr}`)),
  );
});
console.log(
  'native Sidequery UI bootstrap, official Quack query, extension-owned notebook files, isolation, shutdown, security, and asset proxy smoke passed',
);
