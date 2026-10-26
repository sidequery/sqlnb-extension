import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const [duckdb, extension] = process.argv.slice(2).map((path) => resolve(path));
assert(duckdb && extension, 'usage: bun scripts/duckdb-ui-smoke.mjs DUCKDB SQLNB_EXTENSION');
const home = mkdtempSync(join(tmpdir(), 'sqlnb-duckdb-ui-test-'));
const directory = join(home, '.duckdb/extension_data/ui');
const database = join(directory, 'ui.db');
const id = 'b7af9310-8cb8-4224-8e84-7db9548362fb';
const literal = (value) => `'${value.replaceAll("'", "''")}'`;
const original = { notebookSerializationFormat: 3, version: 2, currentDatabase: 'analytics',
  viewMode: { mode: 'default' }, future: { preserved: true }, cells: [
    { cellId: 2, query: "-- ordinary SQL comment\nselect 'π; quoted' as value;", useDatabase: 'analytics', future: 42 },
  ] };
function run(sql, error) {
  const result = spawnSync(duckdb, ['-unsigned', '-init', '/dev/null', '-json', ':memory:', '-c',
    `load ${literal(extension)}; ${sql}`], { env: { ...process.env, HOME: home }, encoding: 'utf8', timeout: 15_000 });
  assert.ifError(result.error);
  if (error) {
    assert.notEqual(result.status, 0, 'expected a database error');
    assert.match(result.stderr, error);
    return;
  }
  assert.equal(result.status, 0, result.stderr);
  return result.stdout.trim() ? JSON.parse(result.stdout) : undefined;
}
try {
  assert.equal(run('select sqlnb_duckdb_ui_notebooks() as notebooks')[0].notebooks, '[]');
  assert.equal(existsSync(database), false, 'browsing an absent source must not create it');
  mkdirSync(directory, { recursive: true });
  run(`attach ${literal(database)} as _duckdb_ui;
    create table _duckdb_ui.main.notebooks(id uuid primary key, name varchar, created timestamp);
    create table _duckdb_ui.main.notebook_versions(notebook_id uuid, version integer, title varchar, json varchar, created timestamp, expires timestamp, primary key(notebook_id,version));
    create table _duckdb_ui.main.current_notebook_id(id uuid);
    insert into _duckdb_ui.main.notebooks values (${literal(id)},'original-internal-name',current_timestamp);
    insert into _duckdb_ui.main.notebook_versions values (${literal(id)},1,'Official fixture',${literal(JSON.stringify(original))},current_timestamp,null);
    insert into _duckdb_ui.main.current_notebook_id values (${literal(id)});
    select sqlnb_duckdb_ui_notebooks() as notebooks`);
  assert.deepEqual(JSON.parse(run('select sqlnb_duckdb_ui_notebooks() as notebooks')[0].notebooks), [
    { id, version: 1, title: 'Official fixture' },
  ]);
  const opened = JSON.parse(run(`select sqlnb_duckdb_ui_notebook(${literal(id)}) as notebook`)[0].notebook);
  assert.deepEqual(JSON.parse(opened.json), original);
  const edited = structuredClone(original);
  edited.cells[0].query = "-- @answer [An edited answer]\n-- ordinary SQL comment\nselect 'edited π; quoted' as value;";
  // Prefer a catalog already attached to this instance, including a nonstandard alias.
  const saved = JSON.parse(run(`attach ${literal(database)} as custom_ui;
    select sqlnb_duckdb_ui_save(${literal(id)},1,'Edited fixture',${literal(JSON.stringify(edited))}) as notebook`)[0].notebook);
  assert.equal(saved.version, 2);
  assert.deepEqual(JSON.parse(saved.json), { ...edited, version: 3 });
  const old = run(`attach ${literal(database)} as ui;
    select count(*) as versions, count(*) filter(where expires is null) as current,
      count(*) filter(where expires between current_timestamp + interval 6 days and current_timestamp + interval 8 days) as retained
    from ui.main.notebook_versions`)[0];
  assert.deepEqual(old, { versions: 2, current: 1, retained: 1 });
  run(`select sqlnb_duckdb_ui_save(${literal(id)},1,'Stale edit',${literal(JSON.stringify(original))})`, /REVISION_CONFLICT/);
  run(`select sqlnb_duckdb_ui_save(${literal(id)},2,'Malformed','{')`, /Invalid DuckDB UI notebook JSON/);
  run(`set enable_external_access=false; select sqlnb_duckdb_ui_notebooks()`, /enable_external_access/);
  const final = run(`attach ${literal(database)} as ui;
    select (select count(*) from ui.main.notebook_versions) as versions,
      (select name from ui.main.notebooks) as name,
      (select id::varchar from ui.main.current_notebook_id) as current`)[0];
  assert.deepEqual(final, { versions: 2, name: 'original-internal-name', current: id });
  const created = JSON.parse(run(`select sqlnb_duckdb_ui_create('Created',${literal(JSON.stringify(original))}) as notebook`)[0].notebook);
  assert.notEqual(created.id, id);
  assert.equal(created.version, 1);
  assert.deepEqual(JSON.parse(created.json), { ...original, version: 1 });
  const copied = JSON.parse(run(`select sqlnb_duckdb_ui_duplicate(${literal(id)},2,'Copied') as notebook`)[0].notebook);
  assert.notEqual(copied.id, id);
  assert.equal(copied.version, 1);
  assert.deepEqual(JSON.parse(copied.json), { ...edited, version: 1 });
  run(`select sqlnb_duckdb_ui_duplicate(${literal(id)},1,'Stale')`, /REVISION_CONFLICT/);
  run(`select sqlnb_duckdb_ui_delete(${literal(id)},1)`, /REVISION_CONFLICT/);
  run(`select sqlnb_duckdb_ui_create('Malformed','{}')`, /Unsupported DuckDB UI notebook format/);
  run(`attach ${literal(database)} as ui; create table ui.main.protected_data as select 42 as answer;`);
  assert.deepEqual(JSON.parse(run(`select sqlnb_duckdb_ui_delete(${literal(id)},2) as deleted`)[0].deleted), { id });
  const remaining = run(`attach ${literal(database)} as ui;
    select (select count(*) from ui.main.notebooks) as notebooks,
      (select count(*) from ui.main.notebook_versions where notebook_id=${literal(id)}::uuid) as deleted_versions,
      (select count(*) from ui.main.current_notebook_id) as selected,
      (select answer from ui.main.protected_data) as answer`)[0];
  assert.deepEqual(remaining, { notebooks: 2, deleted_versions: 0, selected: 0, answer: 42 });
  run(`select sqlnb_duckdb_ui_delete(${literal(id)},2)`, /REVISION_CONFLICT/);
  assert.equal(JSON.parse(run(`select sqlnb_duckdb_ui_notebook(${literal(copied.id)}) as notebook`)[0].notebook).title, 'Copied');
  console.log('DuckDB UI source: read/save/create/duplicate/delete/conflict/rollback/metadata/access checks passed');
} finally {
  rmSync(home, { recursive: true, force: true });
}
