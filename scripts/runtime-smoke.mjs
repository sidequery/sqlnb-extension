import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
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
  'usage: runtime-smoke.mjs --duckdb PATH [--repository PREFIX | --extension PATH --quack-extension PATH_OR_core --httpfs-extension PATH_OR_core]',
);
if (!repository) {
  assert(statSync(extension).size > 0, `empty SQLNB extension: ${extension}`);
  if (quackExtension !== 'core') {
    assert(statSync(quackExtension).size > 0, `empty Quack extension: ${quackExtension}`);
  }
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
      // SQLNB comes from this repository; its runtime dependencies come from core.
      statements.push('install sqlnb from sidequery', 'load sqlnb from sidequery');
    } else {
      statements.push('install quack from core', 'load quack from core');
    }
  } else {
    if (quackExtension !== 'core') {
      statements.push(`load ${sqlLiteral(quackExtension)}`);
    } else if (!includeUI) {
      statements.push('install quack from core', 'load quack from core');
    }
    if (includeUI) statements.push(`load ${sqlLiteral(extension)}`);
  }
  return statements.join('; ');
}

function sqlLiteral(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

// SQLNB has to load into a process that has loaded nothing before it: what its
// own code needs from the system must be linked into it, not found by chance
// in an extension loaded earlier.
{
  const statements = [];
  if (repository) {
    const state = mkdtempSync(join(tmpdir(), 'sidequery-ui-extensions.'));
    statements.push(
      `set extension_directories = [${sqlLiteral(join(state, 'installed'))}]`,
      'install sqlnb from sidequery',
      'load sqlnb from sidequery',
    );
  } else {
    statements.push(`load ${sqlLiteral(extension)}`);
  }
  statements.push("select count(*) from duckdb_extensions() where extension_name = 'sqlnb' and loaded");
  const alone = spawnSync(duckdb, [...cliOptions, ':memory:', '-csv', '-noheader', '-c', statements.join('; ')], {
    encoding: 'utf8',
    timeout: queryTimeoutMs,
    env: cliEnvironment(),
  });
  assert.equal(alone.status, 0, `SQLNB did not load on its own: ${alone.stderr || alone.stdout}`);
  assert.equal(alone.stdout.trim().split('\n').at(-1), '1', alone.stdout);
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

const childHome = mkdtempSync(join(tmpdir(), 'sidequery-ui-cli-home.'));
const child = spawn(duckdb, [...cliOptions, ':memory:', '-json'], {
  stdio: ['pipe', 'pipe', 'pipe'],
  env: cliEnvironment(childHome),
});
// The result cache follows the platform cache directory of the CLI's HOME.
const resultSessions = join(
  process.platform === 'darwin'
    ? join(childHome, 'Library', 'Caches')
    : process.env.XDG_CACHE_HOME || join(childHome, '.cache'),
  'sidequery', 'sqlnb-extension', 'results',
);
function resultFiles() {
  if (!existsSync(resultSessions)) return [];
  return readdirSync(resultSessions).flatMap((session) => readdirSync(join(resultSessions, session)));
}
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

// For a result of more than one row: the CLI prints one row per line, and
// runSQL reads a line.
async function runRows(sql) {
  const directory = mkdtempSync(join(tmpdir(), 'sidequery-ui-rows.'));
  const file = join(directory, 'rows.json');
  try {
    child.stdin.write(`.once ${file}\n${sql};\n`);
    await runSQL('select 1 as done');
    return JSON.parse(readFileSync(file, 'utf8'));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

// For a statement whose own output is not a row to assert on.
async function runStatement(sql) {
  child.stdin.write(`.output /dev/null\n${sql};\n.output stdout\n`);
  await runSQL('select 1 as done');
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
    // The final CLI can emit the opening JSON bracket before a streaming
    // statement throws. Keep expected failures from corrupting the next row.
    child.stdin.write(`.output /dev/null\n${sql};\n.output stdout\n`);
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
if (repository || quackExtension === 'core') {
  assert.equal(firstValue(await runSQL("select loaded from duckdb_extensions() where extension_name = 'quack'")), false);
} else {
  assert.equal(firstValue(await runSQL('select count(*) as count from quack_server_list()')), 0);
}
assert.equal(
  firstValue(await runSQL('create table shared_table as select 7::integer as value; select true')),
  true,
);
assert.equal(
  firstValue(await runSQL('create temp table caller_temp as select 9::integer as value; select true')),
  true,
);
// Instant SQL is available on the final 2.0 binding API. An AT expression is
// evaluated at bind time, before DuckDB rejects time travel on native tables.
const hasReadPolicy = firstValue(await runSQL(
  "select count(*) from duckdb_functions() where function_name = 'sqlnb_read_query'",
)) > 0;
if (hasReadPolicy) {
  assert.equal(firstValue(await runSQL(
    "create sequence instant_policy_sequence; select nextval('instant_policy_sequence')",
  )), 1);
  const unsafeRead = "select * from shared_table at (version => nextval('instant_policy_sequence'))";
  assert.equal(firstValue(await runSQL(`select sqlnb_is_read_only(${sqlLiteral(unsafeRead)})`)), false);
  await expectSQLError(
    `select * from sqlnb_read_query(${sqlLiteral(unsafeRead)})`,
    /SQLNB_READ_ONLY/,
  );
  assert.equal(firstValue(await runSQL("select currval('instant_policy_sequence')")), 1);
  assert.deepEqual(
    await runSQL("select * from sqlnb_read_query('select * from shared_table')"),
    [{ value: 7 }],
  );
}
const startRows = await runSQL(
  `select sqlnb_start(assets := ${sqlLiteral(fixture)}, open_browser := false, notebook_root := ${sqlLiteral(notebookRoot)}, mcp := true) as status`,
);
const start = JSON.parse(firstValue(startRows));
assert.equal(start.engine, 'native-duckdb');
assert.equal(start.protocol, 'quack');
if (repository || quackExtension === 'core') {
  assert.equal(
    firstValue(await runSQL("select installed_from from duckdb_extensions() where extension_name = 'quack'")),
    'core',
    'SQLNB must use the official core Quack',
  );
}
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
if (firstValue(await runSQL('select version()')) === 'v2.0.0') {
  // Submit/QueryResultStream must stop a read at its row limit instead of
  // materializing a trillion rows. Both MCP SQL entrypoints share this path.
  for (const execute of [false, true]) {
    const streamed = sqlToolValue(await mcpSQL('select i::integer as n from range(1000000000000) t(i)', execute, 1));
    assert.deepEqual(streamed.rows, [{ n: 0 }]);
    assert.equal(streamed.truncated, true);
  }
  sqlToolValue(await mcpSQL('create table mcp_returning_check (value integer)', true));
  const inserted = sqlToolValue(await mcpSQL('insert into mcp_returning_check select i from range(10000) t(i) returning value', true, 1));
  assert.equal(inserted.rows.length, 1);
  assert.equal(inserted.truncated, true);
  assert.deepEqual(sqlToolValue(await mcpSQL('select count(*)::integer as n from mcp_returning_check')).rows, [{ n: 10000 }]);
}
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

// Result cache (docs/results.md), present with the final 2.0 Arrow scan API.
const hasResultCache = firstValue(await runSQL(
  "select count(*) from duckdb_functions() where function_name = 'sqlnb_result_tee'",
)) > 0;
if (hasResultCache) {
  const resultId = (number) => number.toString(16).padStart(32, '0');
  const large = resultId(1);
  const small = resultId(2);
  const source = "select i, i * 1.5 as amount, 'c-' || (i % 7) as label from range(50000) t(i)";
  // The statement returns only its first rows, in order; every row is stored.
  const head = await runRows(`select * from sqlnb_result_tee((${source}), id := '${large}', meta := '{"cell":1}')`);
  assert(head.length >= 10240 && head.length < 10240 + 2048, `${head.length} head rows`);
  assert(head.every((row, position) => row.i === position), 'the head is not the first rows');
  assert.deepEqual(await runSQL(`select * from sqlnb_result_tee((select 1 as one), id := '${small}')`), [{ one: 1 }]);
  // The tee marks the statement whose result is kept, so it has to be that
  // statement's whole output.
  for (const [number, misplaced] of [
    `select count(*) from sqlnb_result_tee((select 1 as one), id := '${resultId(90)}')`,
    `create table misplaced as select * from sqlnb_result_tee((select 1 as one), id := '${resultId(91)}')`,
    `explain select * from sqlnb_result_tee((select 1 as one), id := '${resultId(92)}')`,
    `select * from sqlnb_result_tee((select 1 as one), id := '${resultId(93)}'), sqlnb_result_tee((select 2 as two), id := '${resultId(94)}')`,
  ].entries()) {
    await expectSQLError(misplaced, number === 3 ? /keeps one result/ : /returns its columns unchanged/);
  }
  assert.equal(firstValue(await runSQL(`select count(*) from sqlnb_results() where id >= '${resultId(90)}'`)), 0);
  assert.equal(firstValue(await runSQL("select count(*) from duckdb_tables() where table_name = 'misplaced'")), 0);
  assert.equal(
    firstValue(await runSQL("select string_agg(column_name || ' ' || column_type, ', ') from (describe select * from sqlnb_results())")),
    'id VARCHAR, rows BIGINT, stored_rows BIGINT, stored_bytes BIGINT, in_memory BOOLEAN, limit_reason VARCHAR, meta VARCHAR, created_ms BIGINT, elapsed_ms BIGINT, rows_exact BOOLEAN, last_used_ms BIGINT, expires_ms BIGINT',
  );
  assert.deepEqual(
    firstValue(await runSQL(`select list(r order by r.id) from (select id, rows, stored_rows, in_memory, limit_reason, meta, rows_exact from sqlnb_results() where id in ('${large}', '${small}')) r`)),
    [
      { id: large, rows: 50000, stored_rows: 50000, in_memory: false, limit_reason: null, meta: '{"cell":1}', rows_exact: true },
      { id: small, rows: 1, stored_rows: 1, in_memory: true, limit_reason: null, meta: '', rows_exact: true },
    ],
  );
  assert.deepEqual(resultFiles().filter((file) => file.endsWith('.arrow')), [`${large}.arrow`]);
  // Reading back returns the input, in order, with its original column types.
  assert.equal(
    firstValue(await runSQL(`select count(*) from ((${source}) except all (select * from sqlnb_result('${large}')))`)),
    0,
  );
  assert.equal(
    firstValue(await runSQL(`select bool_and(i = position) and count(*) = 50000 from sqlnb_result('${large}', row_number := 'position')`)),
    true,
  );
  assert.equal(
    firstValue(await runSQL(`select list(column_type) = (select list(column_type) from (describe ${source})) from (describe select * from sqlnb_result('${large}'))`)),
    true,
  );
  assert.deepEqual(
    await runSQL(`select count(*) as n, min(i) as low, max(position) as high from sqlnb_result('${large}', first_row := 45000, row_count := 200, row_number := 'position')`),
    [{ n: 200, low: 45000, high: 45199 }],
  );
  await expectSQLError(`select * from sqlnb_result('${resultId(99)}')`, /SQLNB_RESULT_MISSING:/);
  await expectSQLError(`select * from sqlnb_result_tee((select 1), id := '${large}')`, /SQLNB_RESULT_EXISTS:/);
  await expectSQLError(
    `select * from sqlnb_result_tee((select 'a'::enum ('a', 'b') as mood), id := '${resultId(3)}')`,
    /SQLNB_RESULT_UNSUPPORTED:/,
  );
  // A failing statement leaves nothing readable and frees its id.
  await expectSQLError(
    `select * from sqlnb_result_tee((select case when i = 40000 then error('tee failed') else i end as i from range(50000) t(i)), id := '${resultId(3)}')`,
    /tee failed/,
  );
  assert.equal(firstValue(await runSQL(`select count(*) from sqlnb_results() where id = '${resultId(3)}'`)), 0);
  assert.deepEqual(resultFiles().filter((file) => file.includes(resultId(3))), []);
  assert.equal((await runSQL(`select * from sqlnb_result_tee((select 1), id := '${resultId(3)}')`)).length, 1);
  // A limit stops storing; stop_at_limit also stops reading. The notebook passes
  // the Instant SQL verdict as a constant scalar call.
  const endless = 'select i from range(100000000) t(i)';
  await runRows(`select * from sqlnb_result_tee((${endless}), id := '${resultId(4)}', max_result_bytes := 100000, stop_at_limit := system.main.sqlnb_is_read_only(${sqlLiteral(endless)}))`);
  await runRows(`select * from sqlnb_result_tee((${endless}), id := '${resultId(5)}', max_result_bytes := 100000, stop_at_limit := false)`);
  const [stopped, counted] = firstValue(await runSQL(`select list(r order by r.id) from (select id, rows, stored_rows, limit_reason, rows_exact from sqlnb_results() where id in ('${resultId(4)}', '${resultId(5)}')) r`))
    .map(({ id, ...result }) => result);
  assert(stopped.rows < 1000000 && stopped.stored_rows > 0 && stopped.stored_rows <= stopped.rows, JSON.stringify(stopped));
  assert.equal(stopped.limit_reason, 'result-bytes');
  assert.equal(stopped.rows_exact, false);
  assert.deepEqual(counted, { rows: 100000000, stored_rows: stopped.stored_rows, limit_reason: 'result-bytes', rows_exact: true });
  assert.equal(
    firstValue(await runSQL(`select count(*) = ${stopped.stored_rows} and max(i) = ${stopped.stored_rows - 1} from sqlnb_result('${resultId(4)}')`)),
    true,
  );
  // The statement runs on all its threads, and its rows are stored in its own
  // order: a scan with a filter, a sort, several Parquet files, and a verified
  // read stopped at the limit while many threads store.
  await runStatement('create table tee_scan as select i, hash(i) as h from range(3000000) t(i)');
  const parquetDirectory = mkdtempSync(join(tmpdir(), 'sidequery-ui-results.'));
  const parquet = join(parquetDirectory, 'part');
  for (const part of [0, 1, 2]) {
    await runStatement(`copy (select i from range(${part * 400000}, ${(part + 1) * 400000}) t(i)) to ${sqlLiteral(`${parquet}${part}.parquet`)} (row_group_size 50000)`);
  }
  const ordered = [
    [10, 'select i, h from tee_scan where i % 3 = 0', 'i = position * 3', 1000000],
    [11, 'select i, h from tee_scan order by i desc', 'i = 2999999 - position', 3000000],
    [12, `select i from read_parquet(${sqlLiteral(`${parquet}*.parquet`)})`, 'i = position', 1200000],
  ];
  for (const [number, statement, inPlace, rows] of ordered) {
    const first = await runRows(`select * from sqlnb_result_tee((${statement}), id := '${resultId(number)}')`);
    assert.deepEqual(
      await runSQL(`select count(*) as n, bool_and(${inPlace}) as ordered from sqlnb_result('${resultId(number)}', row_number := 'position')`),
      [{ n: rows, ordered: true }],
      statement,
    );
    assert.deepEqual(
      first.map((row) => row.i),
      (await runRows(`select i from sqlnb_result('${resultId(number)}', row_count := ${first.length})`)).map((row) => row.i),
      `the head is not the first stored rows: ${statement}`,
    );
    assert.equal(firstValue(await runSQL(`select sqlnb_result_drop('${resultId(number)}')`)), true);
  }
  // Rows without an order of their own: the head is still the first rows stored.
  const joined = 'select a.i, b.h from tee_scan a join tee_scan b using (i) where a.i % 5 = 0';
  const firstJoined = await runRows(`select * from sqlnb_result_tee((${joined}), id := '${resultId(13)}')`);
  assert(firstJoined.length >= 10240, `${firstJoined.length} head rows of a join`);
  assert.deepEqual(
    firstJoined.map((row) => row.i),
    (await runRows(`select i from sqlnb_result('${resultId(13)}', row_count := ${firstJoined.length})`)).map((row) => row.i),
    'the head of a join is not the first stored rows',
  );
  assert.deepEqual(
    await runSQL(`select count(*) as n, count(distinct i) as different, sum(i) as total from sqlnb_result('${resultId(13)}')`),
    await runSQL(`select count(*) as n, count(distinct i) as different, sum(i) as total from (${joined})`),
  );
  await runRows(`select * from sqlnb_result_tee((select i, h from tee_scan), id := '${resultId(14)}', max_result_bytes := 4000000, stop_at_limit := true)`);
  assert.deepEqual(
    await runSQL(`select count(*) > 0 as kept, bool_and(i = position) as ordered, count(*) = (select stored_rows from sqlnb_results() where id = '${resultId(14)}') as counted from sqlnb_result('${resultId(14)}', row_number := 'position')`),
    [{ kept: true, ordered: true, counted: true }],
  );
  for (const number of [13, 14]) {
    assert.equal(firstValue(await runSQL(`select sqlnb_result_drop('${resultId(number)}')`)), true);
  }
  await runStatement('drop table tee_scan');
  rmSync(parquetDirectory, { recursive: true, force: true });
  // The agent connection cannot reach the cache, on either SQL tool.
  for (const sql of [
    'select * from sqlnb_results()',
    `select * from sqlnb_result('${small}')`,
    `select sqlnb_result_drop('${small}')`,
    `select * from sqlnb_result_tee((select 1), id := '${resultId(6)}')`,
  ]) {
    for (const execute of [false, true]) {
      const rejected = await mcpSQL(sql, execute);
      assert.equal(rejected.isError, true, `agent reached the result cache: ${sql}`);
      assert.equal(rejected.structuredContent.error.code, 'POLICY_BLOCKED');
    }
  }
  // Quack sessions (the notebook's path) share the process-wide cache.
  const quackTee = spawnSync(duckdb, [...cliOptions, ':memory:', '-csv', '-noheader', '-c', [
    loadExtensions(false),
    `select count(*), min(i), max(i) from quack_query(${sqlLiteral(`quack:${endpoint.hostname}:${endpoint.port}`)}, ${sqlLiteral(`select * from sqlnb_result_tee((select * from range(30000) t(i)), id := '${resultId(7)}')`)}, token = ${sqlLiteral(token)})`,
  ].join('; ')], { encoding: 'utf8', timeout: queryTimeoutMs, env: cliEnvironment() });
  assert.equal(quackTee.status, 0, quackTee.stderr || quackTee.stdout);
  assert.equal(quackTee.stdout.trim(), '10240,0,10239');
  assert.equal(firstValue(await runSQL(`select rows from sqlnb_results() where id = '${resultId(7)}'`)), 30000);
  assert.equal(firstValue(await runSQL(`select sqlnb_result_drop('${large}')`)), true);
  assert.equal(firstValue(await runSQL(`select sqlnb_result_drop('${large}')`)), false);
  assert.deepEqual(resultFiles().filter((file) => file.includes(large)), []);
  await expectSQLError(`select * from sqlnb_result('${large}')`, /SQLNB_RESULT_MISSING:/);
  assert.equal(firstValue(await runSQL(`select count(*) from sqlnb_results() where id = '${small}'`)), 1);
  // A result is kept for a period after its last use: 24 hours unless the tee
  // names another, NULL for as long as DuckDB runs. Reading it starts the
  // period again; listing it does not.
  const usage = async (number) => (await runSQL(
    `select last_used_ms, expires_ms from sqlnb_results() where id = '${resultId(number)}'`,
  ))[0];
  const dayMs = 24 * 60 * 60 * 1000;
  const hourMs = 60 * 60 * 1000;
  const pause = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
  assert.equal(firstValue(await runSQL(`select expires_ms - last_used_ms from sqlnb_results() where id = '${small}'`)), dayMs);
  await runRows(`select * from sqlnb_result_tee((select i from range(30000) t(i)), id := '${resultId(20)}', ttl_seconds := 3600)`);
  await runSQL(`select * from sqlnb_result_tee((select 1 as one), id := '${resultId(21)}', ttl_seconds := NULL)`);
  const stored = await usage(20);
  assert.equal(stored.expires_ms - stored.last_used_ms, hourMs);
  assert.equal((await usage(21)).expires_ms, null);
  await pause(20);
  assert.deepEqual(await usage(20), stored);
  assert.equal(firstValue(await runSQL(`select count(*) from sqlnb_result('${resultId(20)}', first_row := 100, row_count := 5)`)), 5);
  const read = await usage(20);
  assert(read.last_used_ms > stored.last_used_ms, 'reading a result did not count as a use');
  assert.equal(read.expires_ms - read.last_used_ms, hourMs);
  // An expired result is gone like an evicted one, with its file.
  await runRows(`select * from sqlnb_result_tee((select i from range(30000) t(i)), id := '${resultId(22)}', ttl_seconds := 1)`);
  assert.deepEqual(resultFiles().filter((file) => file.includes(resultId(22))), [`${resultId(22)}.arrow`]);
  await pause(1200);
  assert.equal(firstValue(await runSQL(`select count(*) from sqlnb_results() where id = '${resultId(22)}'`)), 0);
  assert.deepEqual(resultFiles().filter((file) => file.includes(resultId(22))), []);
  await expectSQLError(`select * from sqlnb_result('${resultId(22)}')`, /SQLNB_RESULT_MISSING:/);
  await expectSQLError(
    `select * from sqlnb_result_tee((select 1), id := '${resultId(23)}', ttl_seconds := 0)`,
    /ttl_seconds must be positive/,
  );
  // A cache limit below what is kept evicts the oldest results as soon as the
  // next result begins, before it stores anything.
  await runSQL(`select * from sqlnb_result_tee((select 1 as one), id := '${resultId(24)}', max_cache_bytes := 0)`);
  assert.deepEqual(
    await runSQL('select id, rows, stored_rows, limit_reason from sqlnb_results()'),
    [{ id: resultId(24), rows: 1, stored_rows: 0, limit_reason: 'cache-bytes' }],
  );
  assert.deepEqual(resultFiles().filter((file) => file.endsWith('.arrow')), []);
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
release = 'two';
// Publishing a new UI must not replace assets beneath an active notebook.
assert.match(await (await fetch(`${remote.baseUrl}/asset.js`)).text(), /sidequeryRemoteFixture = 'one'/);
assert.equal(firstValue(await runSQL('select sqlnb_stop()')), true);
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
// Closing the database removes the process's result session.
assert.deepEqual(existsSync(resultSessions) ? readdirSync(resultSessions) : [], []);
console.log(
  'native SQLNB bootstrap, named/default options, compatibility aliases, official Quack query, notebook history/restore, scoped Git commits/push, result cache, isolation, shutdown, security, and asset proxy smoke passed',
);
