# SQLNB extension for DuckDB

A native DuckDB shell for Sidequery's remotely deployed notebook UI. SQL executes through Quack; an in-process Axum server owns local notebook files. This repository contains native extension code and native tests. UI source and build tooling are maintained separately.

## Remote UI and updates

Configure the stable Sidequery UI URL once. Each `sidequery_ui_start` checks its `manifest.json`, downloads a complete new generation, verifies file sizes and SHA-256 hashes, and activates it in the local disk cache. A failed download or unavailable server falls back to a verified cached generation, including an older release if the active cache is damaged. The first launch needs a reachable compatible UI release.

The running server keeps its chosen generation, so an update cannot mix assets or reload a notebook with unsaved edits. Stop and start the UI to pick up a newly deployed release. Normal UI updates do not require rebuilding or reinstalling the extension.

Sidequery's UI publishing path is `https://app.sidequery.dev/sqlnb/`. Its publishing integration is being delivered separately; this repository does not deploy it.

## Build and run

```sh
bash scripts/build-and-test.sh
.artifacts/osx_arm64/duckdb -unsigned
```

The native build requires Rust, Bun (for native smoke tests), CMake, Ninja, Git, and a C/C++ toolchain. It fetches pinned DuckDB, Quack, httpfs, and vcpkg sources. Use the corresponding platform artifact directory on Linux or Intel macOS.

Load the three matching artifacts in the DuckDB session, using absolute paths:

```sql
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/httpfs.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/quack.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/sidequery_ui.duckdb_extension';
select sidequery_ui_start('https://app.sidequery.dev/sqlnb/', '', 0, true, '~/Documents/sqlnb');
```

The database process must remain running. `sidequery_ui_status()` reports its local URL; `sidequery_ui_stop()` stops the UI server. Notebook listing, saving, folders, and moves use the extension's local file API without browser filesystem permissions.

The authenticated `GET /api/v1/recent-notebook` endpoint returns `{ "path": "relative.sqlnb" }`, or `{ "path": null }` when no readable recent notebook is available. The UI records a successful open or save using `PUT` to the same endpoint with `{ "path": "relative.sqlnb" }`. The path must identify an existing notebook within the configured root. This preference survives server restarts and port changes and is isolated by canonical notebook root under the OS cache's `sidequery/sqlnb-extension/notebooks` directory. It contains only the relative path; unavailable or corrupt preferences do not affect normal notebook file operations.

## Deployment contract

The stable base URL serves `manifest.json` with `version` (7–64 lowercase hexadecimal characters), `bridgeVersion: 1`, an optional relative `assetBase` directory, and a sorted `files` array of `{path, sha256, size}`. File paths are relative to the asset base and include `index.html`. Publish the manifest together with its complete immutable generation. Incompatible bridge versions are rejected; capabilities beyond the native bridge require an extension update.

The cache is scoped by the configured URL under `~/Library/Caches/sidequery/sqlnb-extension/ui` on macOS and `${XDG_CACHE_HOME:-~/.cache}/sidequery/sqlnb-extension/ui` on Linux.

On Linux, an absolute `XDG_CACHE_HOME` works even when `HOME` is unset. Empty or relative XDG paths fall back to `$HOME/.cache`.

New releases live in a separate `managed-v1` directory. Cache cleanup retains the current release, one verified fallback, and releases leased by running extension instances. Obsolete managed releases are removed after their leases are released. Older cache directories are kept as read-only offline fallbacks, since older extension processes do not support leases and may still be using them.

For development, the second argument to `sidequery_ui_start` accepts an absolute local asset directory and takes precedence over the remote URL. No UI source checkout or frontend build is required by the installed extension.

## Private repository alpha build

The stable build above remains pinned to DuckDB 1.5.4. For the private repository,
use the official `v2.0.0-alpha41489` CLI (source `10de957379`):

```sh
scripts/build-alpha.sh --duckdb /absolute/path/to/alpha41489/duckdb
```

This macOS build compiles only Quack and `sidequery_ui` against that exact source,
applies its upstream Quack compatibility patches, and runs the Rust and native
smoke suites, including the live deployed UI check. It fetches and verifies pinned
OpenSSL 3.6.3, then builds its static libraries for the same macOS 11.0 deployment
target as the extensions and Rust server. Curl, Perl, and Make are required in
addition to the normal build tools. Build concurrency defaults to two; set
`CMAKE_BUILD_PARALLEL_LEVEL` to change it. Outputs are retained
in `.artifacts/v2.0.0-alpha41489/<platform>/`. The matching official `httpfs` is
installed from `core`; it is not republished by Sidequery. The output includes
source/dependency provenance and SHA-256 checksums.

After signing and preparing the repository with the Sidequery monorepo's
`tools/extension-repository` tool, validate the signed distribution:

```sh
bun scripts/runtime-smoke.mjs --duckdb /absolute/path/to/alpha41489/duckdb \
  --repository https://extensions.sidequery.dev
```

A local repository directory is also accepted. Each client uses fresh extension
and trust directories, allows only signed extensions, and installs Quack and
`sidequery_ui` from Sidequery. The suite checks queries, notebook files, caller
isolation, authentication, server cleanup, and remote UI generation updates.
