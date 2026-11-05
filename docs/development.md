# Build and test

[Back to README](../README.md)

The native extension requires Rust, Bun (for smoke tests), CMake, Ninja, Git, and
a C/C++ toolchain. The build scripts fetch pinned dependencies. No UI source
checkout or frontend build is required.

Run the commands below from the repository root.

## Rust tests

```sh
mbx test --manifest-path http/Cargo.toml --locked -- --test-threads=1
```

`mbx` is the Boxington compiler cache. If it is unavailable, use `cargo` in its
place. The release build also runs the ignored deployed-UI check, which requires
network access.

## DuckDB 2.0 on macOS

Install Rust 1.99.0 through rustup, have Curl, Perl, and Make available, and supply
the matching official DuckDB 2.0 CLI:

```sh
scripts/build-release.sh --duckdb /absolute/path/to/v2.0.0/duckdb
```

This builds Quack and SQLNB, runs Rust and native smoke tests, and writes artifacts
to `.artifacts/v2.0.0/<platform>/`. See [release builds](releases.md) for exact
versions, prerequisites, and signing procedures.

For local testing on Apple Silicon, start that same CLI with `-unsigned`, then:

```sql
load '/absolute/path/to/sqlnb-extension/.artifacts/v2.0.0/osx_arm64/quack.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/v2.0.0/osx_arm64/sqlnb.duckdb_extension';
select sqlnb_start();
```

Use `osx_amd64` for Intel macOS. Unsigned loading is only needed for local build
artifacts; installed releases use the signed repository.

## Windows qualification

The [Windows workflow](../.github/workflows/windows.yml) runs the Rust contracts
and filesystem security tests, then builds DuckDB 2.0, Quack, and SQLNB and exercises
the native HTTP bridge. From a Visual Studio developer PowerShell with Rust 1.99.0,
Bun, CMake, Ninja, and Git available:

```powershell
./scripts/qualify-windows.ps1
```

The script uses a temporary build directory. The Rust security tests also require
permission to create symbolic links, as configured by the workflow.

## Legacy DuckDB 1.5.4 build

`scripts/build-and-test.sh` targets **DuckDB 1.5.4**, independently of the 2.0
release profile. It supports macOS and Linux, requires Python 3 in addition to the
tools above, and retains a matching CLI and extensions:

```sh
bash scripts/build-and-test.sh
.artifacts/osx_arm64/duckdb -unsigned
```

Use the corresponding platform directory on Linux or Intel macOS. In that CLI:

```sql
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/httpfs.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/quack.duckdb_extension';
load '/absolute/path/to/sqlnb-extension/.artifacts/osx_arm64/sqlnb.duckdb_extension';
select sqlnb_start();
```

Do not mix artifacts between DuckDB versions. The legacy `sidequery_ui_start`,
`sidequery_ui_status`, and `sidequery_ui_stop` compatibility aliases remain available.

## Working on the UI

UI source and build tooling live separately from this repository. On DuckDB 2.0,
point the native server at a built frontend directory:

```sql
select sqlnb_start(assets := '/absolute/path/to/assets');
```

Local `assets` take precedence over the remote `ui` URL. See the
[asset contract](ui-assets.md) for remote hosting and cache behavior.
