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

### Notebook history and project Git

The native HTTP server records notebook source snapshots synchronously on accepted
saves, including the previous on-disk source before replacement. Identical saves
are deduplicated; an explicit restore always appends a revision. A → B → A keeps
all three states. Restores require the current file revision, just like saves.
Renaming through SQLNB keeps the notebook's earlier history accessible at its new
path. Each snapshot records the current project branch and HEAD when available.

History is a private, independent bare Git repository, without alternates or links
to the project's object store. It neither stages files nor changes the user's
branches or index. Embedded, vendored libgit2 provides history even when the Git
executable is absent. State lives beneath
`~/Library/Application Support/sidequery/sqlnb-history/<root-hash>` on macOS and
`$XDG_STATE_HOME/sidequery/sqlnb-history/<root-hash>` on Linux (falling back to
`~/.local/state`). The canonical notebook-root path scopes the hash, so separate
worktrees have separate histories. This is durable application state, not the UI
asset cache. The directory is private to the current user.

Each `refs/snapshots/<commit>` names an independent Git commit containing the exact
source bytes at `source`. Standard Git tools can inspect/export snapshots with
`git --git-dir=<scope>/snapshots.git show <commit>:source`. `catalog.json` supplies
ordered revision metadata, rename associations and retention settings. It is not
SQLite. Snapshot objects and a durable intent journal precede the atomic notebook
replacement; successful requests also finalize the catalog. Startup recovers
unfinished intents as **interrupted-write**, which may represent an intended write
that never reached disk. Such a revision does not claim a successful save. Disk
reconciliation records the actual current source separately when different.

The server reconciles the secured notebook tree on startup and every two seconds,
and joins this task during shutdown. Polling deliberately avoids OS watcher limits
and overflow gaps. External edits are best effort: transient states changed between
polls cannot be recovered, and external renames appear as independent paths. A durable
last-observed source hash prevents unchanged files from being recaptured after
retention removes their snapshots; each scan prunes only once. All
internal saves are captured synchronously, independently of polling. Directory and
file access retains the descriptor-based no-symlink boundary. As with ordinary file
saves, a noncooperating external writer can race the final compare/rename; portable
filesystems do not offer cross-process compare-and-swap replacement.

Authenticated endpoints are `GET /api/v1/history?path=...`,
`GET /api/v1/history/revision?path=...&id=...`,
`POST /api/v1/history/restore {path,id,expectedRevision}` and
`PUT /api/v1/history/settings {maxAgeDays,maxRevisions,maxBytes}`. Defaults are
30 days, 1,000 revisions and 256 MiB per notebook root. The byte budget counts
retained source bytes before Git compression, not filesystem allocation or Git
metadata; valid budgets are 16 MiB through 1 TiB. Retention removes the oldest
revisions, truncates snapshot refs, explicitly packs retained objects and removes
obsolete packs/loose objects. Reducing limits deletes history. The newest eligible
snapshot is retained even if it alone exceeds a byte budget. Count/age limits still
apply. A list/read is scoped to its notebook lineage; an arbitrary object ID cannot
read another notebook's history.

Project Git operations are separate, explicit actions using the installed Git
executable and normal configured authentication, hooks and signing. Authenticated
`GET /api/v1/git` reports repository state, HEAD and an `indexRevision` token;
`GET /api/v1/git/diff?path=...&staged=...` previews a notebook. Stage, unstage and
commit requests include `expectedHead` and `expectedIndex` to reject stale reviewed
state. Commits start from the explicitly selected staged notebook paths, preserving
unrelated staging and unstaged bytes. Configured hooks can change the commit: if a
hook includes unselected paths, the API reports that the commit was created with
additional paths instead of reporting normal success. Newly committed index entries
are reconciled, while pre-existing unrelated staging is preserved; an overlap with
that staging is explicitly reported for inspection. The commit and hook edits are
never rolled back. Push uses only the configured upstream, never force. Repository
initialization is explicit. Git paths stay within the configured notebook root,
even when its repository is an ancestor directory.
