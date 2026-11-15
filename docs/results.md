# Result cache

[Back to README](../README.md)

The notebook keeps every statement's complete result so it can show the first rows
immediately and later page through, aggregate or export the rest without running
the statement again. These functions exist on DuckDB 2.0 builds only.

## Storing a result

```sql
select * from sqlnb_result_tee(
    (select * from orders),
    id := '5f0c2a3d9b8e4f61a7d2c0e9b4a1f387',
    meta := '{"cell":"c1"}'
);
```

| Parameter | Default | Meaning |
|-----------|---------|---------|
| first argument | | The statement whose result is kept, as a table. |
| `id` | required | 32 lowercase hexadecimal characters, unique among the process's results. |
| `head_rows` | 10240 | Rows returned to the caller. |
| `head_bytes` | 4 MiB | Arrow buffer bytes returned to the caller. |
| `meta` | `''` | Caller data returned by `sqlnb_results()`, at most 64 KiB. |
| `max_result_bytes` | 4 GiB | Bytes stored for this result. |
| `max_cache_bytes` | 16 GiB | Bytes stored for all results of the process. |
| `stop_at_limit` | `false` | Stop reading the input once storing stops. |

The tee marks the statement whose result is kept. That statement has to return
the tee's columns unchanged, as `select * from sqlnb_result_tee(...)` does;
anything else, such as an aggregate over the tee, `CREATE TABLE AS`, `EXPLAIN`
or a second tee, is an error before the statement runs.

The statement returns only its first rows: whole chunks, as DuckDB produces
them, until `head_rows` rows or `head_bytes` bytes have been returned (the chunk
that crosses either is still returned whole). Every row is stored. The rows
returned are exactly the first rows stored.

The statement runs as it would without the tee, on all its threads, and each
thread stores the rows it produces. They are stored in the statement's order:
the order of an `ORDER BY`, of a table, or of the files and row groups of a
Parquet scan. Where DuckDB gives a statement's rows no order, such as the
output of a join or an aggregate, or everything when
`preserve_insertion_order` is false, they are stored in the order they arrive.

A result becomes readable when its statement finishes. A statement that fails or
is interrupted leaves nothing behind and frees its id.

### How the order is kept

DuckDB numbers the batches a parallel scan hands to its threads, and delivers a
statement's rows in batch order. Each thread stores Arrow batches labelled with
the executor batch they come from and writes them to the file wherever there is
room, without waiting for earlier rows. When the statement ends, the file's
footer lists the batches in executor order, and readers follow the footer.

Only the first rows need their order while the statement still runs. The thread
on the lowest executor batch returns its rows at once. Every other thread keeps
a copy of the first rows of its batch, at most `head_rows` rows or `head_bytes`
bytes, until the batches before it have ended or enough rows were returned.

Results with at most `head_rows` rows and 1 MiB of Arrow buffers stay in memory.
Larger results are written to `<id>.arrow.tmp` and renamed to `<id>.arrow` once
complete, as an [Arrow IPC file](https://arrow.apache.org/docs/format/Columnar.html#ipc-file-format)
whose footer lets a read seek directly to the batches it needs.

### Limits

When a result would exceed `max_result_bytes`, or the process's results would
exceed `max_cache_bytes` even after evicting the oldest other results, the tee stops
storing. The result keeps the rows before the first batch that was not stored, a
prefix of the statement's rows, and `limit_reason` says why. Batches are stored
whole, and threads store later parts of the result while earlier parts are
still being produced, so the prefix can be shorter than the limit by what the
threads had in progress. The statement still runs to completion, and `rows`
counts every row it produced. Failures to store, such as a full disk or an
unwritable cache directory, likewise stop storing with `limit_reason = 'io'`; they
never fail the statement. Limits count Arrow buffer bytes; a file can exceed its
limit by its footer and the framing of its batches.

With `stop_at_limit := true` the tee also stops reading its input once storing
stops, as a `LIMIT` would, and publishes the stored prefix. `rows` is then the
number of rows read, a lower bound, and `rows_exact` is false. The option accepts
any constant expression, such as Instant SQL's verdict that a statement is safe
to stop early:

```sql
select * from sqlnb_result_tee(
    (select * from big_table),
    id := '5f0c2a3d9b8e4f61a7d2c0e9b4a1f387',
    stop_at_limit := system.main.sqlnb_is_read_only('select * from big_table')
);
```

## Reading results

`sqlnb_results()` lists the readable results, newest first:

| Column | Type | Meaning |
|--------|------|---------|
| `id` | VARCHAR | The result's id. |
| `rows` | BIGINT | Rows the statement produced (or read, when `rows_exact` is false). |
| `stored_rows` | BIGINT | Rows that can be read back. Less than `rows` after a limit. |
| `stored_bytes` | BIGINT | File size, or Arrow buffer bytes for results in memory. |
| `in_memory` | BOOLEAN | Whether the result is held in memory rather than a file. |
| `limit_reason` | VARCHAR | `result-bytes`, `cache-bytes`, `io`, or NULL when every row was stored. |
| `meta` | VARCHAR | The tee's `meta`. |
| `created_ms` | BIGINT | Unix time in milliseconds when the statement started. |
| `elapsed_ms` | BIGINT | Milliseconds from the start of the statement to the end of its input. |
| `rows_exact` | BOOLEAN | False when `stop_at_limit` stopped the statement early. |

`sqlnb_result(id, first_row := 0, row_count := NULL, row_number := NULL)` reads a
stored result back with its original column names, types and row order.
`first_row` and `row_count` select a window, NULL `row_count` meaning every row
from `first_row` on. `row_number` names an extra BIGINT column with each row's
zero-based position in the whole result; it must differ from the result's
columns. Only the batches and columns a query needs are read:

```sql
select * from sqlnb_result('5f0c2a3d9b8e4f61a7d2c0e9b4a1f387',
    first_row := 45000, row_count := 200, row_number := 'position');
select sum(amount) from sqlnb_result('5f0c2a3d9b8e4f61a7d2c0e9b4a1f387');
```

Columns with the same name come back with DuckDB's usual suffixes (`a`, `a_1`).
`TIMESTAMP WITH TIME ZONE` values are stored in UTC.

`sqlnb_result_drop(id)` removes a readable result and returns whether it existed.
Scans that already started, including scans of evicted results, finish reading
their rows.

## Errors

DuckDB prefixes these messages with the error type, such as `Invalid Input Error:`.

| Prefix | Raised by |
|--------|-----------|
| `SQLNB_RESULT_EXISTS:` | A tee whose id is already used by a running or readable result. |
| `SQLNB_RESULT_MISSING:` | Reading a result that never existed, was dropped or was evicted. |
| `SQLNB_RESULT_UNSUPPORTED:` | A tee over a column the cache cannot read back unchanged. |
| `POLICY_BLOCKED:` | Any of these functions on the MCP agent connection. |

Every DuckDB type is stored and read back unchanged, including nested types,
`UNION`, `UUID`, `JSON`, `HUGEINT`, `BIGNUM`, `VARIANT` and `GEOMETRY`, except
`ENUM` (anywhere in a column's type) and aliased types without an Arrow extension
type. Cast those columns, for example to `VARCHAR`, before the tee.

## Files and lifetime

Results belong to the process: every connection and database in it shares them,
including the notebook's Quack sessions. The MCP agent connection cannot use them.
Results are removed when the last database that loaded SQLNB closes.

Files live in a directory of their own per process, created when the first result
is written to disk, under:

- macOS: `~/Library/Caches/sidequery/sqlnb-extension/results/`
- Linux: `${XDG_CACHE_HOME:-~/.cache}/sidequery/sqlnb-extension/results/`
- Windows: `%LOCALAPPDATA%\Cache\sidequery\sqlnb-extension\results\`

The process holds a lock on its directory's `session.lock` file while it runs,
and on Unix the directory and its files are readable by their owner only. A
directory whose lock is free and more than a minute old belonged to a process
that exited without cleaning up; it is removed when another process creates its
own directory.

## Storage format

Files are uncompressed Arrow IPC with batches of at most 8192 rows; a batch
ends earlier when it reaches 64 MiB or its thread's executor batch ends. Batches
are in the file in the order they were written, which is not the result's
order: read the file through its footer, as `sqlnb_result` and any Arrow file
reader do, not as a stream.

The first rows reach the caller before anything is written
to disk, so compression would not change how quickly they appear. Measured on an
Apple Silicon Mac with a warm page cache, with one thread storing the result,
for 5,000,000 rows of `BIGINT`,
`DECIMAL`, `VARCHAR` and `TIMESTAMP WITH TIME ZONE` (medians of three runs):

| | Uncompressed | LZ4 frames |
|---|---|---|
| File size | 239 MB | 92 MB |
| Tee statement (`create temp table`: 308 ms) | 283 ms | 387 ms |
| Window of 200 rows at row 4,500,000 | 1 ms | 1 ms |
| `sum` of one column | 16 ms | 23 ms |
| `count(*)` | 12 ms | 18 ms |

The tee adds about 60 µs to a one-row statement, 80 µs at 1000 rows and 0.2 ms at
10,000 rows, which stay in memory.
