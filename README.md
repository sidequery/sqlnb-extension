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

Keep DuckDB running while you use the browser. SQLNB loads the official Quack,
`httpfs`, and `icu` dependencies from DuckDB core automatically. The first launch
needs an internet connection to download the UI; subsequent launches can use a
verified cached copy.

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

To serve SQLNB through a trusted HTTPS reverse proxy, configure the exact browser
origin, including any non-default port and without a trailing slash:

```sql
select sqlnb_start(
    port := 4567,
    open_browser := false,
    browser_origin := 'https://notebooks.example.net:8443'
);
```

SQLNB still binds to loopback. Forward the origin's entire URL path to the
configured loopback port and preserve the browser's `Host` and `Origin` headers.
The returned launch URL uses the configured HTTPS origin and includes the
session token. Allow only trusted users to reach the proxy.

For a stable browser URL, pass `browser_auth_file := '/path/to/auth.json'` along
with `browser_origin`. The file can configure either or both sign-in methods:

```json
{
  "accessKeySha256": "<lowercase SHA-256 of a 64-character lowercase hex key>",
  "trustedProxy": {
    "header": "X-Authenticated-User",
    "value": "owner@example.net"
  }
}
```

Generate an access key with `openssl rand -hex 32`, store it securely, and put
only its hash in the JSON. Access-key sign-in remembers the browser for seven
days. With a trusted proxy, the ordinary URL opens the notebook for the
specified identity. The proxy must authenticate the user and remove incoming
copies of the identity header before setting its own. Keep untrusted clients
away from the loopback listener. SQLNB tries an existing sign-in automatically
and shows the access-key form if that method is configured and a browser still
needs to sign in. Deployments with neither method use launch links. A valid
launch link can still open SQLNB with any configuration.

To connect an MCP client, start with `mcp := true` and follow the
[MCP setup guide](docs/mcp.md). MCP is disabled by default.

## Development and reference

- [Build and test](docs/development.md) — native prerequisites and version-specific builds.
- [Notebook storage](docs/notebooks.md) — files, DuckDB UI notebooks, history, and project Git.
- [MCP reference](docs/mcp.md) — authentication, tools, and execution behavior.
- [Instant SQL read policy](docs/instant-sql.md) — what previews automatically and why the rest needs Run.
- [Result cache](docs/results.md) — storing and reading back complete notebook results.
- [UI assets](docs/ui-assets.md) — custom frontends, updates, and the deployment contract.
- [Release procedures](docs/releases.md) — maintainer builds, signing, and publication.

## License

The native extension is [MIT licensed](LICENSE). The separately hosted Sidequery
UI is not included in this repository or covered by its license.
