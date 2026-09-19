# SQLNB extension for DuckDB

A native DuckDB shell for Sidequery's remotely deployed notebook UI. SQL executes through Quack; an in-process Axum server owns local notebook files. This repository contains native extension code and native tests. UI source and build tooling are maintained separately.

## Remote UI and updates

`sqlnb_start()` uses the production SQLNB UI by default. Each start checks its `manifest.json`, downloads a complete new generation, verifies file sizes and SHA-256 hashes, and activates it in the local disk cache. A failed download or unavailable server falls back to a verified cached generation, including an older release if the active cache is damaged. The first launch needs a reachable compatible UI release.

The running server keeps its chosen generation, so an update cannot mix assets or reload a notebook with unsaved edits. Stop and start the UI to pick up a newly deployed release. Normal UI updates do not require rebuilding or reinstalling the extension.

Sidequery's UI publishing path is `https://app.sidequery.dev/sqlnb/`. Its publishing integration is being delivered separately; this repository does not deploy it.

With the Sidequery extension repository configured, the installed SQLNB flow is:

```sql
install sqlnb from sidequery;
load sqlnb from sidequery;
select sqlnb_start();
```

On supported DuckDB alpha builds, SQLNB installs and loads Quack from the same trusted Sidequery repository when it starts. It also loads the matching core `httpfs` and `icu` extensions through DuckDB's normal core autoloading policy, so those dependencies do not need separate `load` statements.

## Build and run

```sh
bash scripts/build-and-test.sh
.artifacts/osx_arm64/duckdb -unsigned
```

The native build requires Rust, Bun (for native smoke tests), CMake, Ninja, Git, and a C/C++ toolchain. It fetches pinned DuckDB, Quack, httpfs, and vcpkg sources and includes DuckDB's ICU extension for date profiles. Use the corresponding platform artifact directory on Linux or Intel macOS.

Load the three matching artifacts in the DuckDB session, using absolute paths:

```sql
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/httpfs.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/quack.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/sqlnb.duckdb_extension';
select sqlnb_start();
```

The database process must remain running. `sqlnb_status()` reports its local URL; `sqlnb_stop()` stops the UI server. Notebook listing, saving, folders, and moves use the extension's local file API without browser filesystem permissions.

On DuckDB 2.0 alpha builds, startup options are named and optional. Override only what you need:

```sql
select sqlnb_start(
  ui := 'https://example.com/sqlnb/',
  port := 4567,
  open_browser := false,
  notebook_root := '~/Documents/notebooks'
);
```

For local UI development, use `assets := '/absolute/path/to/assets'`; local assets take precedence over `ui`. Existing `sidequery_ui_start(...)`, `sidequery_ui_status()`, and `sidequery_ui_stop()` calls remain available as compatibility aliases.

The authenticated `GET /api/v1/recent-notebook` endpoint returns `{ "path": "relative.sqlnb" }`, or `{ "path": null }` when no readable recent notebook is available. The UI records a successful open or save using `PUT` to the same endpoint with `{ "path": "relative.sqlnb" }`. The path must identify an existing notebook within the configured root. This preference survives server restarts and port changes and is isolated by canonical notebook root under the OS cache's `sidequery/sqlnb-extension/notebooks` directory. It contains only the relative path; unavailable or corrupt preferences do not affect normal notebook file operations.

## Deployment contract

The stable base URL serves `manifest.json` with `version` (7–64 lowercase hexadecimal characters), `bridgeVersion: 1`, an optional relative `assetBase` directory, and a sorted `files` array of `{path, sha256, size}`. File paths are relative to the asset base and include `index.html`. Publish the manifest together with its complete immutable generation. Incompatible bridge versions are rejected; capabilities beyond the native bridge require an extension update.

The cache is scoped by the configured URL under `~/Library/Caches/sidequery/sqlnb-extension/ui` on macOS and `${XDG_CACHE_HOME:-~/.cache}/sidequery/sqlnb-extension/ui` on Linux.

On Linux, an absolute `XDG_CACHE_HOME` works even when `HOME` is unset. Empty or relative XDG paths fall back to `$HOME/.cache`.

New releases live in a separate `managed-v1` directory. Cache cleanup retains the current release, one verified fallback, and releases leased by running extension instances. Obsolete managed releases are removed after their leases are released. Older cache directories are kept as read-only offline fallbacks, since older extension processes do not support leases and may still be using them.

No UI source checkout or frontend build is required by the installed extension.

## Private repository alpha build

The stable build above remains pinned to DuckDB 1.5.4. For the private repository,
use the official `v2.0.0-alpha41489` CLI (source `10de957379`):

```sh
scripts/build-alpha.sh --duckdb /absolute/path/to/alpha41489/duckdb
```

This macOS build compiles only Quack and `sqlnb` against that exact source,
applies its upstream Quack compatibility patches, and runs the Rust and native
smoke suites, including the live deployed UI check. It fetches and verifies pinned
OpenSSL 3.6.3, then builds its static libraries for the same macOS 11.0 deployment
target as the extensions and Rust server. Curl, Perl, and Make are required in
addition to the normal build tools. Build concurrency defaults to two; set
`CMAKE_BUILD_PARALLEL_LEVEL` to change it. Outputs are retained
in `.artifacts/v2.0.0-alpha41489/<platform>/`. The matching official `httpfs` is
installed from `core`; it is not republished by Sidequery. The output includes
source/dependency provenance and SHA-256 checksums.

Starting the notebook also requires ICU for timezone conversion in date profiles.
The server loads it before accepting queries, using DuckDB's normal extension
autoloading policy. If ICU is not bundled with your DuckDB runtime, install the
matching extension with `install icu from core; load icu;` before starting the
notebook when automatic installation is disabled. Installation requires a
writable extension directory. ICU remains an official DuckDB dependency and is
not republished by Sidequery.

After signing and preparing the repository with the Sidequery monorepo's
`tools/extension-repository` tool, validate the signed distribution:

```sh
bun scripts/runtime-smoke.mjs --duckdb /absolute/path/to/alpha41489/duckdb \
  --repository https://extensions.sidequery.dev
```

A local repository directory is also accepted. Each client uses fresh extension
and trust directories, allows only signed extensions, and proves that starting
`sqlnb` bootstraps Quack from Sidequery. The suite checks queries, notebook files, caller
isolation, authentication, server cleanup, and remote UI generation updates.

## Local MCP server

MCP is disabled by default. On DuckDB alpha, enable it explicitly:

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

On the stable compatibility interface, the sixth positional argument enables MCP:

```sql
select sidequery_ui_start(
  'https://app.sidequery.dev/sqlnb/', '', 0, true,
  '~/Documents/notebooks', true
);
```

The server exposes these tools:

| Tool | Arguments and behavior |
| --- | --- |
| `query` | `sql`, optional `max_rows` (1–1000). Executes one SQL statement; results have a 1 MiB limit and a 30-second deadline. SQL can modify data. |
| `inspect_schema` | Lists database schema columns visible to the agent connection, under the same result bounds. |
| `list_notebooks` | Lists saved notebook paths beneath the configured root. |
| `read_notebook` | `notebook_path`; returns saved source and revision. |
| `write_notebook` | `notebook_path`, `source`, `expected_revision`; an empty revision creates a file, an existing file requires its current revision. |
| `list_live_notebooks` | Returns `{sessions: [...]}` containing each browser's `session_id`, `notebook_path` and current `revision`. |
| `execute_live_notebook` | `session_id`, `expected_revision`, `operation`, optional `arguments`; targets exactly one connected browser document. |

SQL tools use one persistent agent connection to the existing DuckDB database,
shared by MCP clients for this server's lifetime. It is separate from the browser's
Quack connection: database objects are shared, but temporary tables, transactions,
and connection settings are not. Native query cancellation interrupts only its
matching query and finishes cleanup before reusing the connection. BIGINT,
DECIMAL, date/time and nested values are returned as strings to preserve precision.

Live operations are `notebook.get`, `notebook.run`, `cell.insert`, `cell.update`,
`cell.delete` and `cell.run`. Address existing cells with `cell_id` or zero-based
`cell_index`; `cell.insert` uses zero-based `position`. Cell edits accept `sql`,
`title`, `display_title`, `connection` and `markdown` as applicable. `notebook.get`
accepts `run: none|missing|all`; edits accept `run: none|cell|affected`, defaulting
to `none`. Edits change the open document and remain unsaved until the user saves.
For example, after obtaining a session's current revision:

```json
{
  "session_id": "<session_id>",
  "expected_revision": "<revision>",
  "operation": "cell.update",
  "arguments": {"cell_index": 0, "sql": "select 42 as answer", "run": "cell"}
}
```

The browser bridge rejects stale revisions and does not replay delivered commands.
A timeout or cancellation stops subsequent cells, but Quack cannot interrupt a
browser query already running. Inspect the document before retrying a mutation.
Saved-file writes reject paths currently reported open by connected browsers;
use live operations for unsaved documents. Browser polling leases expire after
five seconds without a heartbeat, and live calls time out after 30 seconds.
