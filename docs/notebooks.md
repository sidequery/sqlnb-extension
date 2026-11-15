# Notebook storage and integrations

[Back to README](../README.md)

## Recent notebook

The authenticated `GET /api/v1/recent-notebook` endpoint returns `{ "path": "relative.sqlnb" }`, or `{ "path": null }` when no readable recent notebook is available. The UI records a successful open or save using `PUT` to the same endpoint with `{ "path": "relative.sqlnb" }`. The path must identify an existing notebook within the configured root. This preference survives server restarts and port changes and is isolated by canonical notebook root under the OS cache's `sidequery/sqlnb-extension/notebooks` directory. It contains only the relative path; unavailable or corrupt preferences do not affect normal notebook file operations.

## Uploaded data files

A browser does not tell a page where a dropped file is stored, so the notebook UI imports a dropped CSV, Parquet or JSON file by uploading a copy. The bootstrap response advertises the endpoint as `"imports": { "endpoint": "/api/v1/import", "maxBytes": 1073741824, "folder": "/absolute/notebook/root/.sqlnb-imports" }`; servers without the field have no upload endpoint.

`PUT /api/v1/import?name=<file name>` takes the raw file bytes as the request body and returns `201 { "path": "<absolute path>", "name": "<saved name>", "bytes": <count> }`. It uses the same capability and origin checks as the file API. Files are saved only in `.sqlnb-imports` directly under the notebook root, which is created on first use with a `.gitignore` of `*` and left out of the notebook listing:

- The name must not contain a path separator or control character. Other characters outside letters, digits, spaces and `-_.()` become `_`, leading dots are removed, and the stem is cut to 120 characters. The extension must be `.csv`, `.tsv`, `.txt`, `.parquet`, `.json`, `.jsonl` or `.ndjson`, optionally followed by `.gz` or `.zst`.
- An existing name is never replaced: the upload is saved as `name (2).csv`, `name (3).csv` and so on. The folder and file names are opened relative to the notebook root's descriptor without following symlinks, so a symlinked `.sqlnb-imports` is refused and a symlink inside it is neither written through nor deleted.
- A declared `Content-Length` above `maxBytes` is refused with `413` before reading; the body is also counted while it streams. Bytes go to a temporary file that is renamed into place only after the whole body arrives and is synced; a refused, failed or aborted upload removes it.

`PUT` also takes an optional `id`, a lowercase hyphenated UUID chosen by the client. An aborted request cannot cancel an upload whose whole body the server already received and saved, so a client that cancels also sends `DELETE /api/v1/import?id=<id>`: it deletes the file if that upload was saved, and otherwise makes the upload fail before it is saved. Publishing and cancelling are decided under one lock, so one of the two always wins cleanly. Ids are used once; the server remembers the latest 1024.

`DELETE /api/v1/import?name=<saved name>` removes one uploaded file by the name the upload returned. Uploaded files otherwise stay until the user deletes them.

## DuckDB UI notebooks

The local DuckDB UI launched by `duckdb -ui` stores its notebooks in a DuckDB
database at `~/.duckdb/extension_data/ui/ui.db`. SQLNB exposes existing notebooks
as a second live source alongside local SQLNB files. Opening a notebook does not
execute it; saving edits writes a new version to that same database.

The browser uses three native SQL functions, each returning a JSON string:

```sql
select sqlnb_duckdb_ui_notebooks();
select sqlnb_duckdb_ui_notebook('notebook-uuid');
select sqlnb_duckdb_ui_save('notebook-uuid', expected_entity_version, 'Title', 'edited notebook JSON');
```

Reads return the official ID, entity version, title and stored notebook JSON.
Listing omits the JSON bodies. Saves require serialization format 3 and both the
expected entity version and unchanged content version. An accepted save advances
both versions and expires the preceding current revision in seven days, matching
the official UI. Conflicts and malformed edits roll back. Notebook identity,
creation time, internal name, current-notebook preference, unknown JSON fields
and historical revisions remain intact. Browsing never creates or migrates a
database, and `enable_external_access` must be enabled.

An official UI catalog already attached to the host instance takes precedence;
otherwise each operation opens the existing default database for its duration.
Normal DuckDB locks apply. A separate process holding a write lock prevents
access; stop that process or run both UIs in the same DuckDB instance. No lock
bypass or database copy is used. The current adapter supports editing existing
notebooks, not creating or deleting notebook records.

The frontend owns conversion through the canonical Rust SQLNB parser and
serializer. Cell IDs, database choices, grouping, comments and SQLNB directives
survive saves. Features the official UI does not implement remain SQLNB source
comments, with a compatibility notice in SQLNB. The official UI must reopen or
refresh a notebook to see saved edits. SQLNB refreshes clean documents and retains
dirty drafts when another editor changes the source.

The native source smoke uses an isolated home directory and database:

```sh
bun scripts/duckdb-ui-smoke.mjs /absolute/path/to/duckdb /absolute/path/to/sqlnb.duckdb_extension
```

## Notebook history and project Git

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
