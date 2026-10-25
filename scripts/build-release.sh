#!/usr/bin/env bash
set -euo pipefail

# Match the official DuckDB release and its tested Quack dependency.
readonly DUCKDB_REVISION=d54cbbe3df17e8e5ce68312f483b4155836112aa
readonly DUCKDB_VERSION=v2.0.0
readonly QUACK_REVISION=f96420ed46aee9c42064bdb99ddfd1230d6c0086
readonly RUST_TOOLCHAIN=1.99.0
readonly OPENSSL_VERSION=3.6.3
readonly OPENSSL_SHA256=243a86649cf6f23eeb6a2ff2456e09e5d77dd9018a54d3d96b0c6bdd6ba6c7f1
extension_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# != 2 || "$1" != --duckdb ]]; then
  echo "Usage: scripts/build-release.sh --duckdb /path/to/official/v2.0.0/duckdb" >&2
  exit 1
fi
duckdb_binary="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
version="$("$duckdb_binary" -init /dev/null -csv -noheader -c 'pragma version')"
if [[ "$version" != "$DUCKDB_VERSION,${DUCKDB_REVISION:0:10},"* ]]; then
  echo "Expected $DUCKDB_VERSION at ${DUCKDB_REVISION:0:10}; got $version" >&2
  exit 1
fi
platform="$("$duckdb_binary" -init /dev/null -csv -noheader -c 'pragma platform')"
case "$platform" in osx_arm64|osx_amd64) ;; *)
  echo "This extension-only release build supports macOS; got $platform" >&2; exit 1;; esac
# Use rustup's distributable standard library. Homebrew's build can target the
# build host's newer macOS even when our own crates target macOS 11.0.
rust_toolchain_bin="$(dirname "$(rustup which --toolchain "$RUST_TOOLCHAIN" cargo)")"
export PATH="$rust_toolchain_bin:$PATH"
jobs="${CMAKE_BUILD_PARALLEL_LEVEL:-2}"
export CARGO_BUILD_JOBS="$jobs"
# Keep C++, Rust, and its native crypto dependencies on the same deployment target.
export MACOSX_DEPLOYMENT_TARGET=11.0
source_dir="$extension_dir/.deps/duckdb-$DUCKDB_REVISION"
build_dir="$extension_dir/build/$DUCKDB_VERSION-$platform"
artifact_dir="$extension_dir/.artifacts/$DUCKDB_VERSION/$platform"
openssl_source_dir="$extension_dir/.deps/openssl-$OPENSSL_VERSION"
openssl_archive="$extension_dir/.deps/openssl-$OPENSSL_VERSION.tar.gz"
mkdir -p "$extension_dir/.deps"
if [[ ! -d "$source_dir/.git" ]]; then
  git init --quiet "$source_dir"
  git -C "$source_dir" remote add origin https://github.com/duckdb/duckdb.git
  git -C "$source_dir" fetch --quiet --depth 1 origin "$DUCKDB_REVISION"
  git -C "$source_dir" checkout --quiet --detach FETCH_HEAD
fi
[[ "$(git -C "$source_dir" rev-parse HEAD)" == "$DUCKDB_REVISION" ]]
[[ -z "$(git -C "$source_dir" status --porcelain)" ]]
if [[ ! -f "$openssl_archive" ]]; then
  curl --fail --location --show-error \
    "https://github.com/openssl/openssl/releases/download/openssl-$OPENSSL_VERSION/openssl-$OPENSSL_VERSION.tar.gz" \
    -o "$openssl_archive"
fi
printf '%s  %s\n' "$OPENSSL_SHA256" "$openssl_archive" | shasum -a 256 --check
if [[ ! -d "$openssl_source_dir" ]]; then
  mkdir -p "$openssl_source_dir"
  tar -xzf "$openssl_archive" --strip-components=1 -C "$openssl_source_dir"
fi
case "$platform" in
  osx_arm64) openssl_target=darwin64-arm64-cc ;;
  osx_amd64) openssl_target=darwin64-x86_64-cc ;;
esac
if [[ ! -f "$openssl_source_dir/Makefile" ]]; then
  (cd "$openssl_source_dir" && perl ./Configure "$openssl_target" no-shared no-tests no-module \
    "-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET")
fi
make -C "$openssl_source_dir" -j "$jobs" build_libs
if command -v mbx >/dev/null 2>&1 && [[ -z "${RUSTC_WRAPPER:-}" ]] && [[ -z "${RUSTC_WORKSPACE_WRAPPER:-}" ]]; then
  mbx test --manifest-path "$extension_dir/http/Cargo.toml" --locked -- --include-ignored --test-threads=1
else
  echo "Boxington unavailable or another wrapper selected; using Cargo for HTTP tests" >&2
  cargo test --manifest-path "$extension_dir/http/Cargo.toml" --locked -- --include-ignored --test-threads=1
fi
cmake -S "$source_dir" -B "$build_dir" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET" \
  -DDUCKDB_EXPLICIT_PLATFORM="$platform" \
  -DDUCKDB_EXTENSION_CONFIGS="$extension_dir/extension_config_release.cmake" \
  -DDUCKDB_EXPLICIT_VERSION="$DUCKDB_VERSION" \
  -DBUILD_EXTENSIONS_ONLY=ON -DEXTENSION_STATIC_BUILD=OFF -DBUILD_UNITTESTS=OFF -DBUILD_SHELL=OFF \
  -DOPENSSL_USE_STATIC_LIBS=TRUE \
  -DOPENSSL_INCLUDE_DIR="$openssl_source_dir/include" \
  -DOPENSSL_CRYPTO_LIBRARY="$openssl_source_dir/libcrypto.a" \
  -DOPENSSL_SSL_LIBRARY="$openssl_source_dir/libssl.a"
[[ "$(git -C "$build_dir/_deps/quack_extension_fc-src" rev-parse HEAD)" == "$QUACK_REVISION" ]]
cmake --build "$build_dir" --target quack_loadable_extension sqlnb_loadable_extension --parallel "$jobs"
# Runtime validation uses the official matching httpfs, which remains in core.
bun "$extension_dir/scripts/runtime-smoke.mjs" --duckdb "$duckdb_binary" \
  --extension "$build_dir/extension/sqlnb/sqlnb.duckdb_extension" \
  --quack-extension "$build_dir/extension/quack/quack.duckdb_extension" --httpfs-extension core
mkdir -p "$artifact_dir"
cp "$build_dir/extension/quack/quack.duckdb_extension" "$artifact_dir/"
cp "$build_dir/extension/sqlnb/sqlnb.duckdb_extension" "$artifact_dir/"
{
  echo "duckdb_version=$DUCKDB_VERSION"
  echo "duckdb_revision=$DUCKDB_REVISION"
  echo "quack_revision=$QUACK_REVISION"
  echo "httpfs_repository=core"
  echo "duckdb_platform=$platform"
  echo "macos_deployment_target=$MACOSX_DEPLOYMENT_TARGET"
  echo "openssl_version=$OPENSSL_VERSION"
  echo "rust_toolchain=$RUST_TOOLCHAIN"
  echo "openssl_source_sha256=$OPENSSL_SHA256"
  echo "native_source_revision=$(git -C "$extension_dir" rev-parse HEAD)"
  if [[ -z "$(git -C "$extension_dir" status --porcelain)" ]]; then
    echo "native_source_dirty=false"
  else
    echo "native_source_dirty=true"
  fi
} > "$artifact_dir/manifest.txt"
(cd "$artifact_dir" && shasum -a 256 quack.duckdb_extension sqlnb.duckdb_extension > SHA256SUMS)
echo "Validated release extensions: $artifact_dir"
