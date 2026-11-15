// Instant SQL read policy contract. runtime-smoke.mjs calls this with its
// DuckDB CLI helpers once the native policy functions are registered.
//
// Accepted shapes must return the same rows as the plain query. Refused
// shapes must name their reason and must leave no trace: sequences, tables,
// files and the HTTP server below observe every side effect they could cause.
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export async function readPolicySmoke({ runSQL, expectSQLError, sqlLiteral, firstValue }) {
  const scratch = mkdtempSync(join(tmpdir(), 'sqlnb-read-policy.'));
  const copyTarget = join(scratch, 'copy.csv');
  const servedDatabase = join(scratch, 'served.duckdb');
  let remoteRequests = 0;
  const server = createServer((request, response) => {
    remoteRequests += 1;
    if (!request.url.endsWith('.duckdb')) {
      response.writeHead(200, { 'content-type': 'text/csv' });
      response.end('value\n1\n');
      return;
    }
    // A DuckDB database file, served with the HEAD and range support httpfs uses.
    const body = readFileSync(servedDatabase);
    const range = /^bytes=(\d+)-(\d*)$/.exec(request.headers.range ?? '');
    if (request.method === 'HEAD' || !range) {
      response.writeHead(200, { 'content-length': body.length, 'accept-ranges': 'bytes' });
      response.end(request.method === 'HEAD' ? undefined : body);
      return;
    }
    const start = Number(range[1]);
    const end = range[2] ? Math.min(Number(range[2]), body.length - 1) : body.length - 1;
    response.writeHead(206, {
      'content-range': `bytes ${start}-${end}/${body.length}`, 'content-length': end - start + 1, 'accept-ranges': 'bytes',
    });
    response.end(body.subarray(start, end + 1));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const remoteCSV = `http://127.0.0.1:${server.address().port}/remote.csv`;
  const remoteDatabase = `http://127.0.0.1:${server.address().port}/served.duckdb`;

  // The CLI harness reads one output line per statement, so the rows travel
  // as one list value. Aggregation does not promise order: compare as a set.
  const canonical = (rows) => (rows ?? []).map((row) => JSON.stringify(row)).sort();
  async function accepts(sql, expected) {
    assert.equal(firstValue(await runSQL(`select sqlnb_is_read_only(${sqlLiteral(sql)}) as allowed`)), true, sql);
    const rows = firstValue(await runSQL(`select list(r) as rows from sqlnb_read_query(${sqlLiteral(sql)}) as r`));
    assert.deepEqual(canonical(rows), canonical(expected), sql);
  }
  async function refuses(sql, reason) {
    assert.equal(firstValue(await runSQL(`select sqlnb_is_read_only(${sqlLiteral(sql)}) as allowed`)), false, sql);
    await expectSQLError(
      `select * from sqlnb_read_query(${sqlLiteral(sql)})`,
      new RegExp(`SQLNB_READ_ONLY: Instant SQL does not preview ${reason}`),
    );
  }
  const sequenceValue = async () => firstValue(await runSQL('select currval(\'policy.counter\') as value'));
  const policyRows = async () => firstValue(await runSQL('select count(*)::integer as count from policy.items'));

  await runSQL(`
    create schema policy;
    create table policy.items as
      select i::integer as id, (i % 2)::integer as grp, ('item' || i)::varchar as label,
             {x: i::integer, y: 'k' || i} as info, map {'k': i::integer} as tags
      from range(1, 5) as r(i);
    create sequence policy.counter;
    create view policy.items_view as select id, label from policy.items where id > 1;
    create view policy.items_view_view as select id * 10 as id10 from policy.items_view;
    create view policy.cte_view as with base as (select id from policy.items) select max(id) as top from base;
    create view policy.remote_view as select * from read_csv(${sqlLiteral(remoteCSV)});
    create view policy.remote_view_view as select * from policy.remote_view;
    create view policy.glob_view as select * from glob(${sqlLiteral(join(scratch, '*'))});
    create view policy.sequence_view as select nextval('policy.counter') as n;
    create macro policy.bump() as nextval('policy.counter');
    create macro policy.bump_rows() as table select nextval('policy.counter') as n;
    create type policy.mood as enum ('calm', 'loud');
    attach ':memory:' as policy_db;
    create schema policy_db.inner_schema;
    create table policy_db.inner_schema.remote_name as select 42::integer as answer;
    create view policy_db.inner_schema.local_view as select answer from remote_name;
    create view policy_db.inner_schema.safe_view as select answer + 1 as next from remote_name;
    -- View bodies bind their own catalog and schema first. Each name below
    -- resolves to a safe relation one way and a URL scan the other way.
    create view main.remote_name as select * from read_csv(${sqlLiteral(remoteCSV)});
    create table main.trap_name as select 1::integer as value;
    create view policy_db.inner_schema.trap_name as select * from read_csv(${sqlLiteral(remoteCSV)});
    create view policy_db.inner_schema.trap_view as select * from trap_name;
    attach ${sqlLiteral(servedDatabase)} as served_source;
    create table served_source.facts as select 7::integer as fact;
    detach served_source;
    -- The same file attached from disk and over HTTP: both are native DuckDB catalogs.
    attach ${sqlLiteral(servedDatabase)} as local_file_db (read_only);
    attach ${sqlLiteral(remoteDatabase)} as remote_file_db (read_only);
    create view policy.remote_file_view as select fact from remote_file_db.facts;
    -- Sampled previews: only tables of at least 1,000,000 rows are sampled.
    create table policy.big as select i::integer as id, (i % 7)::integer as grp from range(1200000) as r(i);
    create view policy.big_view as select id from policy.big;
    create view policy.sampled_view as select id from policy.items using sample 50%;
    select nextval('policy.counter') as initialized`);
  for (let level = 0; level <= 8; level += 1) {
    const source = level === 0 ? 'policy.items' : `policy.depth_${level - 1}`;
    await runSQL(`create view policy.depth_${level} as select id from ${source}; select ${level} as created`);
  }
  // Creating the URL-backed views bound them once; only later requests count.
  const requestsAfterSetup = remoteRequests;
  assert.equal(await sequenceValue(), 1);

  // 1. CTEs, casts and window functions over otherwise-allowed inputs.
  await accepts(
    'with picked as (select id, label from policy.items where id < 3) select * from picked order by id',
    [{ id: 1, label: 'item1' }, { id: 2, label: 'item2' }],
  );
  await accepts(
    `with first_step as (select id from policy.items),
          second_step as (select id * 2 as doubled from first_step)
     select sum(doubled)::integer as total from second_step where doubled > (select min(id) from first_step)`,
    [{ total: 20 }],
  );
  // A CTE may shadow a table name; the table it shadows is verified as well.
  await accepts('with items as (select 5 as id) select id from items', [{ id: 5 }]);
  // A CTE named like a URL is a CTE: nothing is fetched.
  await accepts(`with "${remoteCSV}" as (select 1 as one) select one from "${remoteCSV}"`, [{ one: 1 }]);
  await accepts(
    `select id::varchar as text_id, cast(id as decimal(5, 2)) as amount, try_cast(label as integer) as parsed,
            [id]::integer[] as ids, {'a': id}::struct(a bigint) as boxed, date '2024-02-03' as day,
            '2024-02-03 04:05:06'::timestamp as moment
     from policy.items where id = 1`,
    [{
      text_id: '1', amount: 1, parsed: null, ids: [1], boxed: { a: 1 }, day: '2024-02-03',
      moment: '2024-02-03 04:05:06',
    }],
  );
  await accepts(
    `select id, row_number() over (partition by grp order by id) as position,
            sum(id) over (order by id rows between 1 preceding and current row)::integer as running,
            lag(id, 1, 0) over (order by id) as previous, rank() over w as ranked
     from policy.items window w as (order by grp) qualify position <= 2 order by id`,
    [
      { id: 1, position: 1, running: 1, previous: 0, ranked: 3 },
      { id: 2, position: 1, running: 3, previous: 1, ranked: 1 },
      { id: 3, position: 2, running: 5, previous: 2, ranked: 3 },
      { id: 4, position: 2, running: 7, previous: 3, ranked: 1 },
    ],
  );

  // Pattern matching, bitwise and list operators bind built-in functions.
  await accepts(
    `select id, label like 'item%' as like_match, label not like '%2' as not_like, label ilike 'ITEM_' as ilike_match,
            label not ilike 'x%' as not_ilike, label glob 'item[13]' as glob_match, label similar to 'item[0-9]' as similar,
            label ~ '3$' as regex, label ^@ 'it' as prefix, label like 'item!%' escape '!' as escaped,
            (id & 1) as bit_and, (id | 8) as bit_or, (id << 2) as shifted_left, (id >> 1) as shifted_right,
            xor(id, 1) as bit_xor, ~id as bit_not, -id as negated, (2 ^ id)::integer as raised,
            [1, 2, id] @> [id] as list_contains, [id] <@ [1, 2, 3] as list_contained, [id] && [2, 4] as list_overlap
     from policy.items where label like 'item%' and id = 3`,
    [{
      id: 3, like_match: true, not_like: true, ilike_match: true, not_ilike: true, glob_match: true, similar: true,
      regex: true, prefix: true, escaped: false, bit_and: 1, bit_or: 11, shifted_left: 12, shifted_right: 1,
      bit_xor: 2, bit_not: -4, negated: -3, raised: 8, list_contains: true, list_contained: true, list_overlap: false,
    }],
  );

  // 2. Views, verified through their definitions with DuckDB's view search path.
  await accepts('select * from policy.items_view_view order by id10', [{ id10: 20 }, { id10: 30 }, { id10: 40 }]);
  await accepts('select top from policy.cte_view', [{ top: 4 }]);
  await accepts('select count(*)::integer as rows from policy.depth_7', [{ rows: 4 }]);
  // Inside these views unqualified names bind policy_db.inner_schema first, not main.
  await accepts('select answer from policy_db.inner_schema.local_view', [{ answer: 42 }]);
  await refuses('select * from policy_db.inner_schema.trap_view', 'the table function \'read_csv\'.*inside view');

  // 3. Dotted references: catalogs, schemas, relation-qualified columns and struct fields.
  await accepts('select next from policy_db.inner_schema.safe_view', [{ next: 43 }]);
  await accepts('select answer from policy_db.inner_schema.remote_name', [{ answer: 42 }]);
  await accepts(
    `select items.id, items.info.x as x, info.y as y, items.tags['k'] as tag, v.label
     from policy.items as items join policy.items_view as v on items.id = v.id where items.id = 2`,
    [{ id: 2, x: 2, y: 'k2', tag: 2, label: 'item2' }],
  );
  await accepts(
    'select policy_db.inner_schema.remote_name.answer from policy_db.inner_schema.remote_name',
    [{ answer: 42 }],
  );
  await accepts('select fact from local_file_db.facts', [{ fact: 7 }]);

  // 4. Refusals, each with its reason and without side effects.
  await refuses('insert into policy.items values (9, 1, \'x\', null, null)', 'INSERT statements');
  await refuses('update policy.items set id = 0', 'UPDATE statements');
  await refuses('delete from policy.items', 'DELETE statements');
  await refuses('create table policy.created (a integer)', 'CREATE statements');
  await refuses('drop table policy.items', 'DROP statements');
  await refuses('pragma version', 'PRAGMA statements');
  await refuses('attach \':memory:\' as policy_other', 'ATTACH statements');
  await refuses('set threads = 1', 'SET statements');
  await refuses(`copy (select 1) to ${sqlLiteral(copyTarget)}`, 'COPY statements');
  await refuses('select 1; select 2', 'more than one statement');
  await refuses(
    'with added as (insert into policy.items values (9, 1, \'x\', null, null) returning id) select * from added',
    'INSERT, UPDATE, DELETE or MERGE inside a query',
  );
  await refuses('select nextval(\'policy.counter\')', 'the function \'nextval\'');
  await refuses(
    'with c as (select id from policy.items) select id, nextval(\'policy.counter\') over () from c',
    'the window function \'nextval\'',
  );
  await refuses('select policy.bump()', 'the qualified or method-style call .*bump');
  await runSQL('set search_path = \'policy,main\'; select true as ok');
  await refuses('select bump()', 'the function \'bump\'');
  await refuses('select * from bump_rows()', 'the table function \'bump_rows\'');
  await refuses('select \'calm\'::mood', 'casts to the type \'mood\'');
  await runSQL('reset search_path; select true as ok');
  await runSQL('create schema shadow; create macro shadow.lower(x) as nextval(\'policy.counter\'); select true as ok');
  await runSQL('set search_path = \'shadow,main\'; select true as ok');
  await refuses('select lower(\'A\')', 'the macro \'lower\'');
  await runSQL('reset search_path; drop schema shadow cascade; select true as ok');
  // An operator is a function name: a macro on the search path replaces it.
  await runSQL(`create schema shadow;
    create macro shadow."~~"(a, b) as nextval('policy.counter') > 0;
    create macro shadow."&"(a, b) as nextval('policy.counter');
    create macro shadow."@>"(a, b) as nextval('policy.counter') > 0;
    select true as ok`);
  await runSQL('set search_path = \'shadow,main\'; select true as ok');
  await refuses('select label from policy.items where label like \'item%\'', 'the macro \'~~\'');
  await refuses('select 1 & 2', 'the macro \'&\'');
  await refuses('select [1] @> [1]', 'the macro \'@>\'');
  await runSQL('reset search_path; drop schema shadow cascade; select true as ok');
  await refuses('select \'a\'::varchar(\'nocase\')', 'casts to the type');
  await refuses('select * from policy.sequence_view', 'the function \'nextval\'.*inside view');
  await refuses(`select * from read_csv(${sqlLiteral(remoteCSV)})`, 'the table function \'read_csv\'');
  await refuses(`select * from '${remoteCSV}'`, '.*which is not a table or view');
  await refuses('select * from policy.remote_view', 'the table function \'read_csv\'.*inside view');
  await refuses('select * from policy.remote_view_view', 'the table function \'read_csv\'.*inside view .* > ');
  await refuses('select * from policy.glob_view', 'the table function \'glob\'.*inside view');
  await refuses('select count(*) from policy.depth_8', 'the view .*more than 8 levels deep');
  // A DuckDB file attached over HTTP is still a "duckdb" catalog; reading it is network I/O.
  await refuses('select fact from remote_file_db.facts', '\'remote_file_db.facts\' from the database \'remote_file_db\', which is stored at a remote location');
  await refuses('select fact from policy.remote_file_view', '\'remote_file_db.facts\' .*which is stored at a remote location.*inside view');
  await refuses(
    'with recursive r(n) as (select 1 union all select n + 1 from r) select * from r',
    'recursive CTEs',
  );
  await refuses('with loop as (select * from loop) select * from loop', '\'loop\', which is not a table or view');
  // A CTE is not in scope inside its own body, where DuckDB would fall back to a replacement scan.
  await refuses(
    `with "${remoteCSV}" as (select * from "${remoteCSV}") select * from "${remoteCSV}"`,
    '.*which is not a table or view',
  );
  await refuses('select list_transform([1], x -> x + 1)', 'the function \'list_transform\'');
  await refuses('select random()', 'the function \'random\'');
  // A single scalar call can build a value of any size; the table function is row-bounded.
  await refuses('select repeat(\'x\', 1000000000)', 'the function \'repeat\'');
  await accepts('select count(*)::integer as n from repeat(7, 3)', [{ n: 3 }]);
  await refuses('select * from policy.items at (version => 1)', 'AT \\(time travel\\) clauses');
  await refuses('select * from (pivot policy.items on grp using sum(id))', 'statements that DuckDB expands into several statements');

  // 5. Sampled previews. The plan is a hint; sqlnb_read_query verifies the
  // statement again and applies a seeded Bernoulli sample to the verified tree.
  const database = firstValue(await runSQL('select current_database() as name'));
  const plan = async (sql) => {
    const value = firstValue(await runSQL(`select sqlnb_sample_plan(${sqlLiteral(sql)}) as plan`));
    return value === null ? null : JSON.parse(value);
  };
  const sampleArguments = (table, percent, catalog = database, schema = 'policy') =>
    `sample_catalog := ${sqlLiteral(catalog)}, sample_schema := ${sqlLiteral(schema)}, ` +
    `sample_table := ${sqlLiteral(table)}, sample_percent := ${percent}`;
  const sampled = async (sql, table, percent) => (await runSQL(
    `select count(*)::integer as rows, sum(id)::bigint as total from sqlnb_read_query(${sqlLiteral(sql)}, ${sampleArguments(table, percent)})`,
  ))[0];
  async function refusesSampled(sql, reason, table = 'big', percent = 10) {
    await expectSQLError(
      `select * from sqlnb_read_query(${sqlLiteral(sql)}, ${sampleArguments(table, percent)})`,
      new RegExp(`SQLNB_READ_ONLY: ${reason}`),
    );
  }

  assert.deepEqual(await plan('select count(*) from policy.big'), {
    catalog: database, schema: 'policy', table: 'big', rows: 1200000, percent: 10, method: 'bernoulli',
  });
  // One relation is sampled; the other inputs of a join are read in full.
  assert.equal((await plan('select count(*) from policy.big join policy.items using (id)'))?.table, 'big');
  assert.equal((await plan('with b as (select id from policy.big) select max(id) from b'))?.table, 'big');
  for (const exact of [
    'select count(*) from policy.items',
    'select count(*) from policy.big a join policy.big b using (id)',
    'select count(*) from policy.big_view',
    'select count(*) from policy.big join policy.big_view using (id)',
    'select count(*) from policy.big using sample 1%',
    'select count(*) from policy.big tablesample 1%',
    'select count(*) from (select id from policy.big) using sample 1%',
    'select count(*) from policy.big join policy.sampled_view using (id)',
    'insert into policy.items select id, 0, \'x\', null, null from policy.big',
    'select nextval(\'policy.counter\'), id from policy.big',
    `select * from policy.big, read_csv(${sqlLiteral(remoteCSV)})`,
  ]) {
    assert.equal(await plan(exact), null, exact);
  }

  const first = await sampled('select id from policy.big', 'big', 10);
  assert.ok(first.rows > 110000 && first.rows < 130000, `a 10% sample of 1,200,000 rows kept ${first.rows}`);
  assert.deepEqual(await sampled('select id from policy.big', 'big', 10), first, 'the fixed seed repeats the sample');
  assert.deepEqual(await sampled('select id from policy.big where id >= 0', 'big', 10), first, 'filters see the same sample');
  const joined = (await runSQL(
    `select count(*)::integer as rows from sqlnb_read_query('select id from policy.big join policy.items using (id)', ${sampleArguments('big', 10)})`,
  ))[0].rows;
  assert.ok(joined <= 4, 'only the sampled table loses rows in a join');

  // Sampling never widens what the policy accepts.
  await refusesSampled('insert into policy.big values (1, 1)', 'Instant SQL does not preview INSERT statements');
  await refusesSampled('select nextval(\'policy.counter\'), id from policy.big', 'Instant SQL does not preview the function \'nextval\'');
  await refusesSampled(`select * from read_csv(${sqlLiteral(remoteCSV)})`, 'Instant SQL does not preview the table function \'read_csv\'');
  await refusesSampled('select fact from remote_file_db.facts', 'Instant SQL does not preview .*stored at a remote location', 'facts');
  // A sample applies only to the statement's sole read of a large table.
  await refusesSampled('select id from policy.items', 'Instant SQL does not preview a sample of .*policy.big\', which must be this statement\'s only read');
  await refusesSampled('select count(*) from policy.big a join policy.big b using (id)', 'Instant SQL does not preview a sample of .*which must be this statement\'s only read');
  await refusesSampled('select count(*) from policy.big_view', 'Instant SQL does not preview a sample of .*which must be this statement\'s only read');
  await refusesSampled('select count(*) from policy.big tablesample 1%', 'Instant SQL does not preview a sample of .*which must be this statement\'s only read');
  await refusesSampled('select id from policy.big', 'Instant SQL does not preview a sample of 100', 'big', 100);
  await expectSQLError(
    'select * from sqlnb_read_query(\'select id from policy.big\', sample_table := \'big\')',
    /SQLNB_READ_ONLY: a sampled preview needs a catalog, schema, table and percent/,
  );

  assert.equal(await sequenceValue(), 1, 'refused statements must not advance sequences');
  assert.equal(await policyRows(), 4, 'refused statements must not write rows');
  assert.equal(remoteRequests, requestsAfterSetup, 'refused statements must not reach the network');
  assert.equal(existsSync(copyTarget), false, 'refused statements must not write files');

  await runSQL(`detach policy_db; detach local_file_db; detach remote_file_db; drop schema policy cascade;
    drop view main.remote_name; drop table main.trap_name; select true as cleaned`);
  await new Promise((resolve) => server.close(resolve));
}
