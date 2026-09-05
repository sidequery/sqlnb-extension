# SQLNB extension for DuckDB

A native DuckDB shell for Sidequery's remotely deployed notebook UI. SQL executes through Quack; an in-process Axum server owns local notebook files. UI source and its build tooling are maintained separately and are not part of this repository.

The extension accepts a deployed HTTPS UI asset URL. Local asset directories are supported for development. Persistent remote asset caching and update checks are being added.

Build the native extension with `bash scripts/build-and-test.sh`. The build requires Rust, Bun (for native smoke tests), CMake, Ninja, Git, and a C/C++ toolchain. Artifacts are written to `.artifacts/<platform>/`.

Start the matching DuckDB CLI with `-unsigned`, load the adjacent `httpfs`, `quack`, and `sidequery_ui` extensions, then call:

```sql
select sidequery_ui_start('https://your-ui-host.example/sqlnb/', '', 0, true, '~/Documents/sqlnb');
```

The database process must remain running. `sidequery_ui_status()` reports its local URL; `sidequery_ui_stop()` stops the UI server.
