# Release builds and publication

[Back to README](../README.md)

These procedures are for maintainers. Signing and publication use the operator
tooling in the separate Sidequery monorepo and require maintainer access.
Building this repository does not require that access.

Run the commands below from the repository root.

## Community repository

The community build uses the standard `Makefile`, `extension_config.cmake`,
and `test/sql` suite. Its CI pins DuckDB 2.0.0 and the matching community tooling;
it builds and tests artifacts without publishing them.

Submit `extensions/sqlnb/description.yml` to
[duckdb/community-extensions](https://github.com/duckdb/community-extensions),
pinning `repo.ref` to a validated commit of this repository. The community
repository builds, signs, and distributes accepted submissions. A passing build
here does not mean the extension has been accepted or published there.

After acceptance and publication, users can install with:

```sql
install sqlnb from community;
load sqlnb;
select sqlnb_start();
```

Quack, `httpfs`, and `icu` load from DuckDB core, including when SQLNB itself was
installed from the Sidequery repository.

## DuckDB 2.0

For official DuckDB `v2.0.0` (source `d54cbbe3df`), use the matching official CLI:

```sh
scripts/build-release.sh --duckdb /absolute/path/to/v2.0.0/duckdb
```

The macOS build pins DuckDB's final source and its tested Quack revision
`f96420ed46aee9c42064bdb99ddfd1230d6c0086`. It builds static OpenSSL 3.6.3,
runs the Rust tests (including the deployed UI check) and native smoke suite,
then retains Quack and SQLNB in `.artifacts/v2.0.0/<platform>/` with source
provenance and SHA-256 checksums. All native code targets macOS 11.0. Build
concurrency defaults to two; set `CMAKE_BUILD_PARALLEL_LEVEL` to change it.
Install Rust 1.99.0 through rustup before building; this profile selects its
distributable standard library so Homebrew's host deployment target cannot leak
into the extension.
The OpenSSL build also requires Curl, Perl, and Make.

Sign the SQLNB artifact using the monorepo's `tools/extension-repository` operator
tool. Run its signed native smoke and browser qualification before publishing
`sqlnb` to `https://extensions.sidequery.dev`. Quack is built for local qualification;
installed SQLNB uses the official core Quack, `httpfs`, and `icu` dependencies.
After publication, verify a fresh signed installation:

```sh
bun scripts/runtime-smoke.mjs --duckdb /absolute/path/to/v2.0.0/duckdb \
  --repository https://extensions.sidequery.dev
```

The legacy 1.5.4 build in [development.md](development.md) and the alpha
profile below remain available for their exact runtimes; their C++ artifacts
cannot be relabeled as final 2.0 builds.

## Legacy DuckDB 2.0 alpha

This compatibility profile targets the official `v2.0.0-alpha41489` CLI (source `10de957379`):

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
`sqlnb` bootstraps Quack from core. The suite checks queries, notebook files, caller
isolation, authentication, server cleanup, and remote UI generation updates.
