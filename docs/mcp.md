# Local MCP server

[Back to README](../README.md)

MCP is disabled by default. On DuckDB 2.0, enable it explicitly:

```sql
select sqlnb_start(mcp := true, notebook_root := '~/Documents/notebooks');
select sqlnb_status();
```

The returned status contains `mcp.enabled`, `mcp.url` (the loopback `/mcp`
Streamable HTTP endpoint), and `mcp.token`. Configure an MCP client with that URL
and an `Authorization: Bearer <mcp.token>` header. The token is separate from the
browser capability, changes at every start, and is available only from the SQL
connection that started the server. Do not put it in a URL. The server rejects
foreign browser origins and Host headers. `sqlnb_stop()` stops both surfaces;
closing the owning SQL connection also stops a server started with MCP enabled.

On the legacy DuckDB 1.5.4 compatibility interface, the sixth positional argument enables MCP:

```sql
select sidequery_ui_start(
  'https://app.sidequery.dev/sqlnb/', '', 0, true,
  '~/Documents/notebooks', true
);
```

The server uses Sidequery's canonical `sidequery.*` tool names and contract 2.0
metadata. This is an intentional wire break: the earlier generic tool names are
removed. Every result includes `ok`; failures set MCP `isError` and include
`error.code` and `error.message`, with both structured content and text fallback.

| Tool | Arguments and behavior |
| --- | --- |
| `sidequery.sql.query` | `sql`, optional `limit` (default 100, 1–1000). Executes one read-only SELECT. |
| `sidequery.sql.execute` | Same arguments; permits one statement including DDL and DML. |
| `sidequery.sql.schema.inspect` | Lists schema columns visible to the agent connection. |
| `sidequery.notebook.sql.list` | Returns saved notebook paths and `sessions` with browser `session_id`, `notebook_path`, and `revision`. |
| `sidequery.notebook.sql.get` | Inspects an open notebook; optional `cell_limit` (default 3, maximum 3), `limit`, `run`, and `bust_cache`. |
| `sidequery.notebook.sql.cell.list` | Lists all cells in the open notebook, including SQL and stable current document IDs. |
| `sidequery.notebook.sql.run` | Runs the open notebook and returns per-cell results. |
| `sidequery.notebook.sql.cell.run` | Runs one cell addressed by `cell_id` or zero-based `cell_index`; optional `limit`, `bust_cache`. |
| `sidequery.notebook.sql.cell.insert` | `sql`, optional zero-based `index`, display `title`, `run`, `limit`. |
| `sidequery.notebook.sql.cell.update` | Cell address, optional `sql`, display `title`, `run`, `limit`. |
| `sidequery.notebook.sql.cell.delete` | Deletes the addressed live cell. |
| `sidequery.notebook.sql.source.read` | Embedded saved-file operation: `notebook_path`; returns source and revision. |
| `sidequery.notebook.sql.source.write` | Embedded saved-file operation: `notebook_path`, `source`, `expected_revision`; empty revision creates, existing files require their current revision. Returns `mutation.saved=true`. |

SQL tools use one persistent agent connection to the existing DuckDB database,
shared by MCP clients for this server's lifetime. It is separate from the browser's
Quack connection: database objects are shared, but temporary tables, transactions,
and connection settings are not. Results have a 1 MiB limit and a 30-second deadline.
`max_rows` is accepted as the hosted spelling of `limit`; conflicting values fail.
Rows are JSON objects, with BIGINT, DECIMAL, date/time and nested values serialized
as strings to preserve precision. Native cancellation interrupts only its matching
query and finishes cleanup before reusing the connection.

The read-only tool checks DuckDB's parsed statement, prepared-statement mutation
properties, and bound plan. It rejects volatile functions and table functions it
cannot establish are read-only. User-defined views and macros are rejected before
binding because they can hide bind-time side effects; use the explicitly mutating
`sidequery.sql.execute` tool when these are needed.

Live tools accept `notebook_path` to select a unique open notebook. Omit it only
when exactly one browser session exists, or provide `session_id` to select a window.
Every edit and explicit run requires `expected_revision` from listing or a previous
result. Reads may omit it; they still validate the selected revision in the browser.
Cell `title` changes the display title, preserving the named SQL result used by
dependent cells. An empty title clears it. Live edits return `mutation.saved=false`
and stay unsaved until the user saves in the browser.

For example, after listing sessions, call `sidequery.notebook.sql.cell.update`:

```json
{
  "session_id": "<session_id>",
  "expected_revision": "<revision>",
  "cell_index": 0,
  "sql": "select 42 as answer",
  "title": "Answer",
  "run": "none"
}
```

Then call `sidequery.notebook.sql.cell.run` with the returned revision and cell
address. Notebook results use the shared `kind: notebook` envelope with operation,
per-cell statuses/results, and explicit mutation state.

Unlike native/hosted Sidequery, embedded reads and edits default to `run=none`.
Quack cannot enforce read-only automatic execution on its browser connection, so
`get` with `run=missing` can reuse cached results, but any requested automatic SQL
execution reports `POLICY_BLOCKED`. `run=all`, `bust_cache=true` with an automatic
run mode, and edit `run=cell|affected` likewise cannot execute SQL. The explicit
notebook/cell run tools execute on the browser connection and permit mutations.

The browser bridge rejects stale revisions and does not replay delivered commands.
A timeout or cancellation stops subsequent cells, but Quack cannot interrupt a
browser query already running. Inspect the document before retrying a mutation.
Saved-file writes reject paths currently reported open by connected browsers.
Browser polling leases expire after five seconds without a heartbeat, and live
calls time out after 30 seconds.

The browser/native MCP bridge negotiates `contract_version: 2`. Old cached UI
builds and older native servers cannot apply commands from this contract: the
bridge refuses incompatible versions before editing. Publish the matching UI and
native extension together; restart the owning SQLNB server to load updated assets.
