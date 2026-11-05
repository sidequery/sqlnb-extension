# SQLNB for DuckDB

SQL notebooks in your browser, backed by your local DuckDB session. Open and save
notebook files, run queries, and work with tables and charts.

This repository contains the native extension: a local HTTP server, notebook file
and history management, and an optional MCP server. SQL runs through Quack. The
Sidequery notebook UI is developed separately and downloaded on first launch;
you don't need to build the frontend to use the extension.

## Install and start

Use DuckDB 2.0 with a matching published SQLNB build for your platform. Register
Sidequery's signed extension repository once:

```sql
set allow_extension_repositories = 'allowed';
create extension repository sidequery
    with prefix 'https://extensions.sidequery.dev';
```

Then install and open the notebook:

```sql
install sqlnb from sidequery;
load sqlnb from sidequery;
select sqlnb_start();
```

Keep DuckDB running while you use the browser. SQLNB loads its Quack, `httpfs`,
and `icu` dependencies automatically. The first launch needs an internet
connection to download the UI; subsequent launches can use a verified cached copy.

Use `select sqlnb_status();` to see the local URL and `select sqlnb_stop();` to
stop the server. Restart the server to pick up UI updates.

## Configuration

Choose a notebook directory or port, and control whether startup opens a browser:

```sql
select sqlnb_start(
    notebook_root := '~/Documents/notebooks',
    port := 4567,
    open_browser := false
);
```

To connect an MCP client, start with `mcp := true` and follow the
[MCP setup guide](docs/mcp.md). MCP is disabled by default.

## Development and reference

- [Build and test](docs/development.md) — native prerequisites and version-specific builds.
- [Notebook storage](docs/notebooks.md) — files, DuckDB UI notebooks, history, and project Git.
- [MCP reference](docs/mcp.md) — authentication, tools, and execution behavior.
- [UI assets](docs/ui-assets.md) — custom frontends, updates, and the deployment contract.
- [Release procedures](docs/releases.md) — maintainer builds, signing, and publication.
