# UI assets and updates

[Back to README](../README.md)

`sqlnb_start()` uses the production SQLNB UI by default. Each start checks its `manifest.json`, downloads a complete new generation, verifies file sizes and SHA-256 hashes, and activates it in the local disk cache. A failed download or unavailable server falls back to a verified cached generation, including an older release if the active cache is damaged. The first launch needs a reachable compatible UI release.

The running server keeps its chosen generation, so an update cannot mix assets or reload a notebook with unsaved edits. Stop and start the UI to pick up a newly deployed release. Normal UI updates do not require rebuilding or reinstalling the extension.

Sidequery's UI publishing path is `https://app.sidequery.dev/sqlnb/`. UI publication is managed separately from this repository.

On DuckDB 2.0, use another compatible deployment or a local frontend build:

```sql
select sqlnb_start(ui := 'https://example.com/sqlnb/');
-- Or serve a local build instead:
select sqlnb_start(assets := '/absolute/path/to/assets');
```

Choose one startup command. Local `assets` take precedence over `ui` when both
are supplied.

## Manifest and cache

The stable base URL serves `manifest.json` with `version` (7–64 lowercase hexadecimal characters), `bridgeVersion: 1`, an optional relative `assetBase` directory, and a sorted `files` array of `{path, sha256, size}`. File paths are relative to the asset base and include `index.html`. Publish the manifest together with its complete immutable generation. Incompatible bridge versions are rejected; capabilities beyond the native bridge require an extension update.

The cache is scoped by the configured URL under `~/Library/Caches/sidequery/sqlnb-extension/ui` on macOS and `${XDG_CACHE_HOME:-~/.cache}/sidequery/sqlnb-extension/ui` on Linux.

On Linux, an absolute `XDG_CACHE_HOME` works even when `HOME` is unset. Empty or relative XDG paths fall back to `$HOME/.cache`.

New releases live in a separate `managed-v1` directory. Cache cleanup retains the current release, one verified fallback, and releases leased by running extension instances. Obsolete managed releases are removed after their leases are released. Older cache directories are kept as read-only offline fallbacks, since older extension processes do not support leases and may still be using them.

No UI source checkout or frontend build is required by the installed extension.
