# Instant SQL read policy

[Back to README](../README.md)

Instant SQL previews the statement under the cursor while the user types, without
an explicit Run. The browser asks the native extension to run it through
`sqlnb_read_query(sql)`, which applies the read policy in
[`sqlnb_read_policy.hpp`](../src/include/sqlnb_read_policy.hpp). `sqlnb_is_read_only(sql)`
reports the same decision as a boolean; it is a hint, never an authorization.

## How a statement is verified

`sqlnb_read_query` parses the SQL with DuckDB's built-in parser (no parser
extensions) and verifies the parsed statement inside the bind of the outer
query, in the same transaction that then binds and runs it. Nothing is
prepared, explained or executed to classify it.

Verification happens before DuckDB binds the statement because binding itself
can run code: AT clauses, type parameters, table function binds, replacement
scans of files and URLs, and macro expansion. Every name is resolved through a
catalog entry retriever configured the way DuckDB's binder configures its own:
the caller's search path at the top level and, inside a view, the view's own
catalog and schema first. Names are checked against the allowlists below before
any catalog lookup, so verification never autoloads or installs an extension.

Anything the verifier does not positively recognise is refused. A refusal is an
error that starts with `SQLNB_READ_ONLY: Instant SQL does not preview` followed by
the reason and, for definitions inside views, the chain of views. The statement
remains available through explicit Run.

## Accepted

- Exactly one `SELECT` statement, including set operations, subqueries, `VALUES`
  and `FROM`-first syntax.
- Non-recursive `WITH` clauses whose bodies are themselves accepted. A CTE is
  visible to later CTEs of the same clause, to the query and to its subqueries,
  never inside its own body. When a CTE shares its name with a table or view, that
  relation must be acceptable too.
- Tables in attached native DuckDB databases that are in memory or stored at a
  local path (including attached database files and temporary tables),
  unqualified or qualified as `schema.table` or
  `database.schema.table`, without generated columns, whose column types are
  `BOOLEAN`, signed and unsigned integers, `FLOAT`, `DOUBLE`, `DECIMAL`, `DATE`,
  `TIME`, `TIMETZ`, `TIMESTAMP` (all precisions and `WITH TIME ZONE`), `INTERVAL`,
  `BLOB`, `UUID` or `VARCHAR` without a collation, or `LIST`, `ARRAY`, `STRUCT` and
  `MAP` of those.
- Views in native DuckDB databases whose definitions pass this policy,
  recursively, up to 8 nested levels and 64 view expansions per statement.
- The table functions `range`, `generate_series`, `unnest` and `repeat`.
- Constants, column references (including relation-qualified columns such as
  `t.col` or `db.schema.t.col` and struct and map field access such as `col.field`),
  comparisons, `AND`/`OR`/`NOT`, `CASE`, `BETWEEN`, `IN`, `IS [NOT] NULL`,
  `COALESCE`, `TRY`, subscripts, slices, list literals and `*` or `COLUMNS(...)`
  without lambdas.
- Calls to an allowlist of side-effect-free scalar and aggregate built-ins
  (arithmetic, rounding, string, pattern-matching, list, struct and date/time
  helpers, and the common aggregates).
- The operators that parse to built-in functions: arithmetic and unary minus,
  `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE` (with or without `ESCAPE`), `GLOB`,
  `SIMILAR TO`, regular-expression `~` and `!~`, `^@` (starts with), the bitwise
  `&`, `|`, `<<`, `>>`, `~` and `xor`, `^` (power), `||`, and the list operators
  `@>`, `<@` and `&&`. DuckDB binds each of these by its function name (`LIKE` is
  `~~`, `GLOB` is `~~~`), so a macro of that name on the search path would replace
  the operator; such a macro is refused like any other.
- Window functions: `row_number`, `rank`, `dense_rank`, `percent_rank`,
  `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`
  and the allowlisted aggregates, with partitions, ordering, frames, `FILTER`,
  named windows and `QUALIFY`.
- `CAST`, `TRY_CAST`, `::` and typed literals to the built-in types listed above,
  with integer literal parameters (for example `DECIMAL(5, 2)` or `INTEGER[3]`).

Every function name, function behind an operator or column reference (field
access, implicit struct packing, SQL value functions such as `current_date`) and
type name must resolve, through the binder's search path, to a DuckDB built-in:
an internal entry from DuckDB itself, its `core_functions` or the ICU extension
SQLNB loads at startup. A user macro, user function or user type that shadows a
built-in name is refused.

## Refused

- Every statement other than a single `SELECT`: DML, DDL, `PRAGMA`, `ATTACH`,
  `SET`, `COPY`, `CALL`, `EXPORT`, PIVOT statements and multiple statements.
- `INSERT`, `UPDATE`, `DELETE`, `MERGE` or `COPY` inside a query or `WITH` clause.
- Recursive CTEs (including `USING KEY`), which can run unbounded before a row
  limit applies.
- Names that are not tables or views in an attached native DuckDB database: files,
  URLs and other replacement scans, and tables or views in attached databases of
  other types.
- Tables and views in a native DuckDB database file attached from a remote
  location (`http://`, `https://`, `s3://`, `hf://` and the other remote
  prefixes DuckDB recognises, or any path with a non-`file:` scheme). Such a
  database still reports the catalog type `duckdb`, but every read is network I/O
  and a preview would repeat it whenever typing pauses. The check uses the
  database's storage path (`Catalog::GetDBPath`), not its name, and applies inside
  views too.
- Any other table function, including file, network and catalog scans
  (`read_csv`, `read_parquet`, `glob`, `query`, `duckdb_*`), table macros and
  `PIVOT`/`UNPIVOT`, `DESCRIBE`, `SHOW` and `SUMMARIZE` in `FROM`.
- Functions outside the allowlist, including volatile or state-changing ones such
  as `nextval`, `setval` and `random`, and the scalar `repeat(string, n)`, which
  can build a value of any size in one call (the `repeat` table function is
  accepted: its rows are bounded); user-defined functions; macros, whose bodies
  are not verified; qualified and method-style calls such as `schema.f()` or
  `x.lower()`.
- Casts to user-defined, extension (`JSON`, `GEOMETRY`) or other unlisted types
  (`ENUM`, `UNION`, `VARIANT`, `BIT`), qualified type names and type parameters
  that are not integer literals (such as a collation).
- Lambdas, `COLLATE`, prepared-statement parameters, `AT` time-travel clauses,
  tables with generated columns and columns of unlisted types.
- Views that reach anything refused here, nest more than 8 levels deep or expand
  more than 64 views.
- A name that the surrounding query defines as a CTE, and any statement while a
  loaded extension rewrites column names during binding.

## Cost

The policy bounds what a preview can do, not how long it runs. The notebook
host bounds that: it fetches at most 200 rows or 1 MiB, and interrupts a preview
that has executed for 2 seconds (Quack `CANCEL_REQUEST` for that query's own id),
showing "Preview stopped after 2,000 ms. Use Run." An explicit Run never has
this budget. Aggregates and window functions over hundreds of millions of rows
finish in tens of milliseconds on current hardware; unbounded shapes such as
`count(*)` over `range(1e12)` stop at the budget. Aggregates that build one large
value (`string_agg` or `list` over a large input) are bounded only by that time.

## Sampled previews

The notebook offers an opt-in setting, "Instant SQL previews: Sampled
(approximate)". With it, a preview that the exact path cannot finish quickly runs
on a sample of one table instead. Previews are exact by default.

- `sqlnb_sample_plan(sql)` returns JSON naming the table a sampled preview would
  sample and the percentage, for example
  `{"catalog":"big","schema":"main","table":"f","rows":50000000,"percent":0.2,"method":"bernoulli"}`,
  or `NULL` when the preview stays exact or the policy refuses the statement. Like
  `sqlnb_is_read_only`, it is a hint.
- `sqlnb_read_query(sql, sample_catalog := …, sample_schema := …, sample_table := …,
  sample_percent := …)` verifies the statement exactly as above, then puts a
  Bernoulli sample with a fixed seed on the verified reference to that table and
  binds the rewritten statement. It refuses the sample, with the reason, unless
  that table is still the statement's sample candidate. Sampling never changes
  what the policy accepts.
- The candidate is the statement's largest table, and only when it has at least
  1,000,000 rows, the whole statement (including the views it reads) reads it
  exactly once and directly in the statement rather than inside a view, and
  nothing in the statement or its views already samples (`TABLESAMPLE`,
  `USING SAMPLE`). One table is sampled; the other inputs of a join are read in
  full, so the disclosed percentage is the one applied.
- The percentage is the largest of 10, 5, 2, 1, 0.5, 0.2, 0.1, 0.05, 0.02, 0.01,
  0.005, 0.002 and 0.001 percent that keeps at least 100,000 expected rows: 10%
  of 1,200,000 rows, 0.2% of 50,000,000.
- The seed is fixed (42), so the same statement samples the same rows on every
  pause in typing, in memory, in attached files and in Parquet-backed tables
  alike. A sample on a base table applies before `WHERE`.

The method was chosen by measurement on DuckDB 2.0.0 (36-core M5 Ultra, a
50,000,000-row table in an attached database file):

| Shape, 4 threads | Exact | Seeded Bernoulli | Unseeded Bernoulli | `SYSTEM` |
| --- | --- | --- | --- | --- |
| Full-table window function | 2.53 s | 0.95 s | 0.30 s | n/a |
| Aggregate over 200,000 groups | 0.38 s | 0.85 s | 0.24 s | n/a |
| Aggregate | 0.53 s | 0.92 s | n/a | 0.03 s |
| Top-N | 0.03 s | 0.88 s | n/a | 0.03 s |

- Seeded Bernoulli is repeatable but runs on one thread: about 0.9 s per
  50,000,000 rows at any thread count. It beats the exact preview only for shapes
  that are slow exactly, which is why the notebook tries the exact preview first
  and samples only when that does not finish within one second. Above about
  110,000,000 rows the sampled scan itself exceeds the 2-second budget.
- Unseeded Bernoulli is parallel but samples different rows each time, so a
  preview would change on every pause.
- `SYSTEM` sampling keeps or drops whole vectors of 2,048 rows. Even with a seed
  it was not repeatable (38,912 and 59,392 rows for the same 1% sample of
  5,000,000 rows), drew those rows from 19 to 29 vectors where Bernoulli spread
  over all 2,442, and returned no rows from a 500-row table. It is not used.
- `RESERVOIR` sampling is repeatable but needs the whole input (about 1.2 s).

## Tests

[`read-policy-smoke.mjs`](../scripts/read-policy-smoke.mjs) runs inside the
runtime smoke test of `scripts/build-release.sh`. It checks each accepted shape
against its rows and each refusal against its reason, and asserts that refused
statements leave sequences, tables, files and a local HTTP server untouched.
