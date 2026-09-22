import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import {
  chmodSync,
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
const repository = values.get('--repository');
assert(
  duckdb && (repository || (extension && quackExtension && httpfsExtension)),
  'usage: runtime-smoke.mjs --duckdb PATH [--repository PREFIX | --extension PATH --quack-extension PATH --httpfs-extension PATH_OR_core]',
);
if (!repository) {
  assert(statSync(extension).size > 0, `empty SQLNB extension: ${extension}`);
  assert(statSync(quackExtension).size > 0, `empty Quack extension: ${quackExtension}`);
  if (httpfsExtension !== 'core') {
    assert(statSync(httpfsExtension).size > 0, `empty httpfs extension: ${httpfsExtension}`);
  }
}

const queryTimeoutMs = repository || httpfsExtension === 'core' ? 120_000 : 20_000;
const cliInit = repository
  ? join(mkdtempSync(join(tmpdir(), 'sidequery-ui-cli-init.')), 'init.sql')
  : '/dev/null';
if (repository) {
  // CREATE emits a result row; keep setup output separate from query assertions.
  writeFileSync(cliInit, [
    '.output /dev/null',
    'set allow_unsigned_extensions = false;',
    "set allow_extension_repositories = 'allowed';",
    `create extension repository sidequery with prefix ${sqlLiteral(repository)};`,
    '.output stdout',
    '',
  ].join('\n'));
}
const cliOptions = ['-init', cliInit, ...(repository ? [] : ['-unsigned'])];
function cliEnvironment(home = mkdtempSync(join(tmpdir(), 'sidequery-ui-cli-home.'))) {
  // DuckDB fixes its repository trust directory at startup under $HOME/.duckdb.
  return { ...process.env, HOME: home };
}
function loadExtensions(includeUI = true) {
  const statements = [];
  if (repository || httpfsExtension === 'core') {
    const state = mkdtempSync(join(tmpdir(), 'sidequery-ui-extensions.'));
    statements.push(`set extension_directories = [${sqlLiteral(join(state, 'installed'))}]`);
    if (!repository || !includeUI) {
      statements.push('install httpfs from core', 'load httpfs from core');
    }
  } else {
    statements.push(`load ${sqlLiteral(httpfsExtension)}`);
  }
  if (repository) {
    if (includeUI) {
      // SQLNB installs and loads its private Quack dependency from this repository.
      statements.push('install sqlnb from sidequery', 'load sqlnb from sidequery');
    } else {
      statements.push('install quack from sidequery', 'load quack from sidequery');
    }
  } else {
    statements.push(`load ${sqlLiteral(quackExtension)}`);
    if (includeUI) statements.push(`load ${sqlLiteral(extension)}`);
  }
  return statements.join('; ');
}

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

const child = spawn(duckdb, [...cliOptions, ':memory:', '-json'], {
  stdio: ['pipe', 'pipe', 'pipe'],
  env: cliEnvironment(),
});
let stdout = '';
let stderr = '';
const waiters = [];
let expectedSQLFailures = 0;
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
      queryTimeoutMs,
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

function expectSQLError(sql, expectedMessage) {
  return new Promise((resolve, reject) => {
    let received = '';
    const timeout = setTimeout(() => {
      child.stderr.off('data', onData);
      reject(new Error(`Expected SQL error was not received for ${sql}: ${received}`));
    }, queryTimeoutMs);
    function onData(chunk) {
      received += chunk;
      if (expectedMessage.test(received)) {
        clearTimeout(timeout);
        child.stderr.off('data', onData);
        expectedSQLFailures += 1;
        resolve();
      }
    }
    child.stderr.on('data', onData);
    child.stdin.write(`${sql};\n`);
  });
}

function firstValue(rows) {
  return Object.values(rows[0])[0];
}

assert.equal(
  firstValue(
    await runSQL(
      `${loadExtensions()}; select true as loaded`,
    ),
  ),
  true,
);
assert.equal(
  firstValue(await runSQL("select sidequery_ui_start(null, '', 0, false) is null as propagated")),
  true,
);
// The old positional entrypoint remains available for existing callers.
for (const functionName of ['sidequery_ui_status', 'sidequery_ui_stop']) {
  assert.equal(
    firstValue(await runSQL(`select count(*) from duckdb_functions() where function_name = '${functionName}'`)),
    1,
  );
}
// Rejected start calls must not poison the database on runtimes that enforce fallibility.
await expectSQLError(
  'select sqlnb_start(port := -1, open_browser := false)',
  /port must be between 0 and 65535/,
);
assert.equal(firstValue(await runSQL('select 42 as recovered')), 42);
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
  `select sqlnb_start(assets := ${sqlLiteral(fixture)}, open_browser := false, notebook_root := ${sqlLiteral(notebookRoot)}, mcp := true) as status`,
);
const start = JSON.parse(firstValue(startRows));
assert.equal(start.engine, 'native-duckdb');
assert.equal(start.protocol, 'quack');
assert.equal(
  firstValue(await runSQL("select loaded from duckdb_extensions() where extension_name = 'icu'")),
  true,
);
// Match the UTC timestamp expression used by source-column date profiles.
// Disable query-triggered autoloading to prove startup supplied the dependency.
assert.equal(
  firstValue(await runSQL("set autoload_known_extensions = false; select cast(to_timestamp(86400) at time zone 'UTC' as varchar)")),
  '1970-01-02 00:00:00',
);
await runSQL('set autoload_known_extensions = true; select true');
const launch = new URL(start.launchUrl);
const token = launch.hash.match(/^#sidequery_token=([0-9a-f]{64})$/)?.[1];
assert(token, `missing launch capability: ${start.launchUrl}`);
assert.equal(
  firstValue(
    await runSQL(
      'select count(*) as count from (select sqlnb_status() as status from range(3)) where status is not null',
    ),
  ),
  3,
);
const headers = { authorization: `Bearer ${token}` };

// Exercise the actual C++ DuckDB bridge, not the HTTP layer's query mock.
assert.equal(start.mcp.enabled, true);
let mcpSession;
let mcpRequestID = 0;
async function mcpRequest(method, params, notification = false) {
  const response = await fetch(start.mcp.url, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${start.mcp.token}`,
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      ...(mcpSession ? { 'mcp-session-id': mcpSession, 'mcp-protocol-version': '2025-03-26' } : {}),
    },
    body: JSON.stringify({ jsonrpc: '2.0', ...(notification ? {} : { id: ++mcpRequestID }), method, params }),
  });
  assert(response.ok, `MCP ${method} returned ${response.status}: ${await response.clone().text()}`);
  mcpSession = response.headers.get('mcp-session-id') ?? mcpSession;
  if (notification) return;
  const body = await response.text();
  const message = response.headers.get('content-type')?.includes('text/event-stream')
    ? JSON.parse(body.split('\n').find((line) => line.startsWith('data: ') && line.slice(6).trim()).slice(6))
    : JSON.parse(body);
  assert.equal(message.error, undefined, JSON.stringify(message.error));
  return message.result;
}
await mcpRequest('initialize', {
  protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'native-runtime-smoke', version: '1' },
});
await mcpRequest('notifications/initialized', {}, true);
async function mcpSQL(sql, execute = false, limit = 100) {
  return mcpRequest('tools/call', {
    name: execute ? 'sidequery.sql.execute' : 'sidequery.sql.query', arguments: { sql, limit },
  });
}
function sqlToolValue(result) {
  assert.notEqual(result.isError, true, JSON.stringify(result));
  return result.structuredContent ?? JSON.parse(result.content.find((item) => item.type === 'text').text);
}
const readResult = sqlToolValue(await mcpSQL('select value from shared_table'));
assert.deepEqual(readResult.rows, [{ value: 7 }]);
assert.deepEqual(sqlToolValue(await mcpSQL("select 'insert; sqlnb_stop()' as text /* delete */")).rows, [{ text: 'insert; sqlnb_stop()' }]);
const limited = sqlToolValue(await mcpSQL('select i::integer as n from range(3) t(i)', false, 1));
assert.deepEqual(limited.rows, [{ n: 0 }]);
assert.equal(limited.truncated, true);
sqlToolValue(await mcpSQL('create table mcp_mutation_check (value integer)', true));
sqlToolValue(await mcpSQL('create sequence mcp_sequence', true));
sqlToolValue(await mcpSQL("create macro mcp_hidden_mutation() as nextval('mcp_sequence')", true));
sqlToolValue(await mcpSQL("create view mcp_hidden_view as select nextval('mcp_sequence') as value", true));
for (const sql of [
  'insert into mcp_mutation_check values (999)',
  'with input as (select 1) insert into mcp_mutation_check select * from input',
  `copy shared_table to ${sqlLiteral(join(notebookRoot, 'must-not-write.csv'))}`,
  'set threads = 1',
  "select nextval('mcp_sequence')",
  'select mcp_hidden_mutation()',
  'select * from mcp_hidden_view',
  'select sqlnb_stop()',
  "select * from quack_serve('quack:127.0.0.1:0')",
]) {
  const rejected = await mcpSQL(sql);
  assert.equal(rejected.isError, true, `read-only SQL was accepted: ${sql}`);
  assert.equal(rejected.structuredContent.error.code, 'POLICY_BLOCKED');
  assert.match(JSON.stringify(rejected), /POLICY_BLOCKED|read-only|mutation|volatile/i);
  assert.deepEqual(sqlToolValue(await mcpSQL('select 42 as recovered')).rows, [{ recovered: 42 }]);
}
assert.deepEqual(sqlToolValue(await mcpSQL('select count(*)::integer as n from mcp_mutation_check')).rows, [{ n: 0 }]);
assert.equal(existsSync(join(notebookRoot, 'must-not-write.csv')), false);
sqlToolValue(await mcpSQL('insert into mcp_mutation_check values (12)', true));
assert.deepEqual(sqlToolValue(await mcpSQL('select value from mcp_mutation_check')).rows, [{ value: 12 }]);
assert.deepEqual(sqlToolValue(await mcpSQL("select nextval('mcp_sequence')::integer as n", true)).rows, [{ n: 1 }]);
for (const execute of [false, true]) {
  const rejected = await mcpSQL('select 1; insert into mcp_mutation_check values (999)', execute);
  assert.equal(rejected.isError, true);
  assert.match(JSON.stringify(rejected), /exactly one/i);
}
assert.deepEqual(sqlToolValue(await mcpSQL('select value from mcp_mutation_check')).rows, [{ value: 12 }]);

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

// Exercise the public history/Git contract through the actual loaded extension.
async function notebookApi(path, method = 'GET', body) {
  const response = await fetch(`${start.baseUrl}/api/v1/${path}`, {
    method,
    headers: { ...headers, 'content-type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const result = await response.json();
  assert.equal(response.status, 200, JSON.stringify(result));
  return result;
}
assert.equal((await fetch(`${start.baseUrl}/api/v1/history?path=first.sqlnb`)).status, 401);
assert.equal((await fetch(`${start.baseUrl}/api/v1/git`)).status, 401);
const historyBefore = await notebookApi('history?path=first.sqlnb');
assert(historyBefore.revisions.length >= 2, 'baseline and accepted save must be retained');
const baselineRevision = historyBefore.revisions.at(-1);
const historicalSource = await notebookApi(`history/revision?path=first.sqlnb&id=${baselineRevision.id}`);
assert.equal(historicalSource.source, first.source);
const restoredFile = await notebookApi('history/restore', 'POST', {
  path: 'first.sqlnb', id: baselineRevision.id, expectedRevision: updated.revision,
});
assert.equal(readFileSync(join(notebookRoot, 'first.sqlnb'), 'utf8'), first.source);
assert.equal((await notebookApi('history?path=first.sqlnb')).revisions[0].reason, 'restore');
await notebookApi('file', 'PUT', {
  path: 'first.sqlnb', source: updatedSource, expectedRevision: restoredFile.revision,
});
const beforeNoop = (await notebookApi('history?path=first.sqlnb')).revisions.length;
await notebookApi('file', 'PUT', {
  path: 'first.sqlnb', source: updatedSource, expectedRevision: updated.revision,
});
assert.equal((await notebookApi('history?path=first.sqlnb')).revisions.length, beforeNoop);

function fixtureGit(args, cwd = notebookRoot) {
  const result = spawnSync('git', ['-C', cwd, ...args], { encoding: 'utf8' });
  assert.equal(result.status, 0, `fixture Git ${args[0]} failed`);
  return result.stdout.trim();
}
await notebookApi('git/init', 'POST', {});
fixtureGit(['config', 'user.name', 'SQLNB smoke']);
fixtureGit(['config', 'user.email', 'sqlnb-smoke@localhost']);
fixtureGit(['config', 'commit.gpgsign', 'false']);
writeFileSync(join(notebookRoot, 'unrelated.txt'), 'keep this staged\n');
fixtureGit(['add', '--', 'unrelated.txt']);
let gitStatus = await notebookApi('git');
assert(gitStatus.available);
assert.equal(gitStatus.head, null);
assert(gitStatus.files.every((entry) => /\.(sql|sqlnb)$/.test(entry.path)));
await notebookApi('git/stage', 'POST', {
  paths: ['first.sqlnb'], expectedHead: gitStatus.head, expectedIndex: gitStatus.indexRevision,
});
gitStatus = await notebookApi('git');
assert.match((await notebookApi('git/diff?path=first.sqlnb&staged=true')).diff, /select 42/);
const initialCommit = await notebookApi('git/commit', 'POST', {
  paths: ['first.sqlnb'], message: 'Initial notebook', expectedHead: gitStatus.head, expectedIndex: gitStatus.indexRevision,
});
assert.equal(fixtureGit(['show', 'HEAD:first.sqlnb']), updatedSource.trim());
assert.equal(fixtureGit(['diff', '--cached', '--name-only']), 'unrelated.txt');
assert.equal(fixtureGit(['rev-parse', 'HEAD']), initialCommit.head);

// A local bare remote verifies push behavior without publishing user work.
const remoteRoot = mkdtempSync(join(tmpdir(), 'sqlnb-smoke-remote.'));
fixtureGit(['init', '--bare'], remoteRoot);
fixtureGit(['remote', 'add', 'origin', remoteRoot]);
fixtureGit(['push', '--set-upstream', 'origin', fixtureGit(['branch', '--show-current'])]);
await notebookApi('file', 'PUT', {
  path: 'first.sqlnb', source: 'select 43;\n', expectedRevision: updated.revision,
});
gitStatus = await notebookApi('git');
await notebookApi('git/stage', 'POST', {
  paths: ['first.sqlnb'], expectedHead: gitStatus.head, expectedIndex: gitStatus.indexRevision,
});
gitStatus = await notebookApi('git');
const nextCommit = await notebookApi('git/commit', 'POST', {
  paths: ['first.sqlnb'], message: 'Update notebook', expectedHead: gitStatus.head, expectedIndex: gitStatus.indexRevision,
});
await notebookApi('git/push', 'POST', { expectedHead: nextCommit.head });
assert.equal(fixtureGit(['rev-parse', `refs/heads/${gitStatus.branch}`], remoteRoot), nextCommit.head);
assert.equal(fixtureGit(['diff', '--cached', '--name-only']), 'unrelated.txt');

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
  loadExtensions(false),
  `attach ${sqlLiteral(`quack:${directEndpoint.hostname}:${directEndpoint.port}`)} as sidequery_direct (token ${sqlLiteral(token)})`,
  'select value from sidequery_direct.main.shared_table',
].join('; ');
const directClient = spawnSync(duckdb, [...cliOptions, ':memory:', '-csv', '-noheader', '-c', directClientSQL], {
  encoding: 'utf8',
  timeout: queryTimeoutMs,
  env: cliEnvironment(),
});
assert.equal(directClient.status, 0, directClient.stderr || directClient.stdout);
assert.equal(directClient.stdout.trim(), '7');

const clientSQL = [
  loadExtensions(false),
  `attach ${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)} as sidequery_native (token ${sqlLiteral(token)})`,
  'select value from sidequery_native.main.shared_table',
].join('; ');
const client = spawnSync(duckdb, [...cliOptions, ':memory:', '-csv', '-noheader', '-c', clientSQL], {
  encoding: 'utf8',
  timeout: queryTimeoutMs,
  env: cliEnvironment(),
});
assert.equal(client.status, 0, client.stderr || client.stdout);
assert.equal(client.stdout.trim(), '7');

const missingTemp = spawnSync(
  duckdb,
  [
    ...cliOptions,
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      loadExtensions(false),
      `from quack_query(${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)}, ${sqlLiteral('select * from caller_temp')}, token = ${sqlLiteral(token)})`,
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: queryTimeoutMs, env: cliEnvironment() },
);
assert.notEqual(missingTemp.status, 0, 'Quack session must not see the caller temporary table');

for (const ownerFunction of ['sqlnb_status', 'sqlnb_stop']) {
  const rejectedOwnerCall = spawnSync(
    duckdb,
    [
      ...cliOptions,
      ':memory:',
      '-csv',
      '-noheader',
      '-c',
      [
        loadExtensions(false),
        `from quack_query(${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)}, ${sqlLiteral(`select ${ownerFunction}()`)}, token = ${sqlLiteral(token)})`,
      ].join('; '),
    ],
    { encoding: 'utf8', timeout: queryTimeoutMs, env: cliEnvironment() },
  );
  assert.notEqual(rejectedOwnerCall.status, 0, `a Quack client must not call ${ownerFunction}`);
  assert.match(
    `${rejectedOwnerCall.stderr}\n${rejectedOwnerCall.stdout}`,
    /only be called from the connection that started SQLNB/,
  );
  assert.equal(firstValue(await runSQL('select 42 as recovered')), 42);
}

const serversBeforeStop = await runSQL('select count(*) as count from quack_server_list()');
assert.equal(firstValue(serversBeforeStop), 1);
assert.equal(firstValue(await runSQL('select sqlnb_stop()')), true);
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
  `select sqlnb_start(ui := 'http://127.0.0.1:${upstreamPort}/notebook/', open_browser := false, notebook_root := ${sqlLiteral(notebookRoot)}) as status`,
);
const remote = JSON.parse(firstValue(remoteRows));
const remoteAsset = await fetch(`${remote.baseUrl}/asset.js`);
assert.equal(remoteAsset.status, 200);
assert.match(await remoteAsset.text(), /sidequeryRemoteFixture = 'one'/);
assert.equal((await fetch(`${remote.baseUrl}/../outside`)).status, 404);
assert(upstreamPaths.includes('/notebook/manifest.json'));
assert(!upstreamPaths.includes('/notebook/outside'));
assert.equal(firstValue(await runSQL('select sqlnb_stop()')), true);
release = 'two';
const updatedUi = JSON.parse(firstValue(await runSQL(
  `select sqlnb_start(ui := 'http://127.0.0.1:${upstreamPort}/notebook/', open_browser := false, notebook_root := ${sqlLiteral(notebookRoot)}) as status`,
)));
assert.match(await (await fetch(`${updatedUi.baseUrl}/asset.js`)).text(), /sidequeryRemoteFixture = 'two'/);
assert.equal(firstValue(await runSQL('select sqlnb_stop()')), true);
await new Promise((resolve) => upstream.close(resolve));
const offline = JSON.parse(firstValue(await runSQL(
  `select sqlnb_start(ui := 'http://127.0.0.1:${upstreamPort}/notebook/', open_browser := false, notebook_root := ${sqlLiteral(notebookRoot)}) as status`,
)));
assert.match(await (await fetch(`${offline.baseUrl}/asset.js`)).text(), /sidequeryRemoteFixture = 'two'/);
assert.equal(firstValue(await runSQL('select sqlnb_stop()')), true);

const externalAccessDisabled = spawnSync(
  duckdb,
  [
    ...cliOptions,
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      loadExtensions(),
      'set enable_external_access = false',
      'select sqlnb_start()',
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: queryTimeoutMs, env: cliEnvironment() },
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
    ...cliOptions,
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [
      loadExtensions(),
      `select sqlnb_start(assets := ${sqlLiteral(fixture)}, open_browser := false)`,
      'select sqlnb_stop()',
    ].join('; '),
  ],
  { encoding: 'utf8', timeout: queryTimeoutMs, env: cliEnvironment(defaultHome) },
);
assert.equal(defaultRootStart.status, 0, defaultRootStart.stderr || defaultRootStart.stdout);
assert.equal(existsSync(join(defaultHome, 'Documents', 'sqlnb')), true);

const zeroArgumentHome = mkdtempSync(join(tmpdir(), 'sidequery-ui-zero-argument-home.'));
const fakeBin = join(zeroArgumentHome, 'bin');
mkdirSync(fakeBin);
writeFileSync(join(fakeBin, 'open'), '#!/bin/sh\nexit 0\n');
chmodSync(join(fakeBin, 'open'), 0o755);
const zeroArgumentStart = spawnSync(
  duckdb,
  [
    ...cliOptions,
    ':memory:',
    '-csv',
    '-noheader',
    '-c',
    [loadExtensions(), 'select sqlnb_start()', 'select sqlnb_stop()'].join('; '),
  ],
  {
    encoding: 'utf8',
    timeout: queryTimeoutMs,
    env: {
      ...cliEnvironment(zeroArgumentHome),
      PATH: `${fakeBin}:${process.env.PATH ?? ''}`,
    },
  },
);
assert.equal(zeroArgumentStart.status, 0, zeroArgumentStart.stderr || zeroArgumentStart.stdout);
assert.equal(existsSync(join(zeroArgumentHome, 'Documents', 'sqlnb')), true);

const compatibilityStart = JSON.parse(firstValue(await runSQL(
  `select sidequery_ui_start('', ${sqlLiteral(fixture)}, 0, false, ${sqlLiteral(notebookRoot)})`,
)));
assert.equal(compatibilityStart.engine, 'native-duckdb');
assert.equal(firstValue(await runSQL('select sidequery_ui_stop()')), true);

child.stdin.end();
await new Promise((resolve, reject) => {
  child.on('exit', (code) =>
    code === (expectedSQLFailures > 0 ? 1 : 0)
      ? resolve()
      : reject(new Error(`DuckDB exited ${code}: ${stderr}`)),
  );
});
console.log(
  'native SQLNB bootstrap, named/default options, compatibility aliases, official Quack query, notebook history/restore, scoped Git commits/push, isolation, shutdown, security, and asset proxy smoke passed',
);
