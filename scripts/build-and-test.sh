#!/usr/bin/env bash

set -euo pipefail

extension_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$extension_dir/scripts/vcpkg_host_environment.sh"

readonly DUCKDB_REVISION="08e34c447bae34eaee3723cac61f2878b6bdf787"
readonly DUCKDB_VERSION="v1.5.4"
readonly QUACK_REVISION="40de7badae4193c29d9c0834473fb76acc6c51e6"
readonly HTTPFS_REVISION="13e18b3c9f3810334f5972b76a3acc247b28e537"
readonly VCPKG_REVISION="84bab45d415d22042bd0b9081aea57f362da3f35"

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    duckdb_platform="osx_arm64"
    vcpkg_triplet="arm64-osx"
    ;;
  Darwin:x86_64)
    duckdb_platform="osx_amd64"
    vcpkg_triplet="x64-osx"
    ;;
  Linux:x86_64)
    duckdb_platform="linux_amd64"
    vcpkg_triplet="x64-linux"
    ;;
  Linux:arm64|Linux:aarch64)
    duckdb_platform="linux_arm64"
    vcpkg_triplet="arm64-linux"
    ;;
  *)
    echo "Unsupported SQLNB build host: $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

for tool in bun cargo cmake git ninja python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required SQLNB build tool: $tool" >&2
    exit 1
  fi
done

task_root="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/sidequery-ui-extension.XXXXXX")"
duckdb_source_dir="$task_root/duckdb"
vcpkg_root="$task_root/vcpkg"
manifest_build_dir="$task_root/manifest"
generated_manifest_dir="$duckdb_source_dir/build/extension_configuration"
build_dir="$task_root/build"
vcpkg_installed_dir="$build_dir/vcpkg_installed"
artifact_dir="$extension_dir/.artifacts/$duckdb_platform"
artifact_parent="$extension_dir/.artifacts"

cleanup() {
  case "$task_root" in
    */sidequery-ui-extension.*) rm -rf -- "$task_root" ;;
    *) echo "Refusing unsafe cleanup path: $task_root" >&2 ;;
  esac
}
trap cleanup EXIT

echo "Testing the in-process Axum server"
if command -v mbx >/dev/null 2>&1 && [[ -z "${RUSTC_WRAPPER:-}" ]] && [[ -z "${RUSTC_WORKSPACE_WRAPPER:-}" ]]; then
  mbx test --manifest-path "$extension_dir/http/Cargo.toml" --locked
else
  cargo test --manifest-path "$extension_dir/http/Cargo.toml" --locked
fi

ensure_checkout() {
  local repository="$1"
  local revision="$2"
  local directory="$3"
  local label="$4"
  local history="${5:-shallow}"
  local fetch_args=()
  if [[ "$history" == "shallow" ]]; then
    fetch_args+=(--depth 1)
  elif [[ "$history" != "full" ]]; then
    echo "Unknown checkout history mode for $label: $history" >&2
    exit 1
  fi
  git init --quiet "$directory"
  git -C "$directory" remote add origin "$repository"
  git -C "$directory" fetch --quiet "${fetch_args[@]}" origin "$revision"
  git -C "$directory" checkout --quiet --detach FETCH_HEAD
  if [[ "$(git -C "$directory" rev-parse HEAD)" != "$revision" ]]; then
    echo "$label checkout did not resolve to $revision" >&2
    exit 1
  fi
}

echo "Fetching DuckDB $DUCKDB_VERSION at $DUCKDB_REVISION"
ensure_checkout "https://github.com/duckdb/duckdb.git" "$DUCKDB_REVISION" "$duckdb_source_dir" "DuckDB"

echo "Fetching vcpkg at $VCPKG_REVISION"
ensure_checkout "https://github.com/microsoft/vcpkg.git" "$VCPKG_REVISION" "$vcpkg_root" "vcpkg" "full"

if [[ ! -x "$vcpkg_root/vcpkg" ]]; then
  run_vcpkg_with_clean_apple_toolchain_environment "$vcpkg_root/bootstrap-vcpkg.sh" -disableMetrics
fi

export VCPKG_ROOT="$vcpkg_root"
export VCPKG_DOWNLOADS="$task_root/vcpkg-downloads"
mkdir -p "$VCPKG_DOWNLOADS" "$generated_manifest_dir"

extension_config="$extension_dir/extension_config.cmake"
toolchain_file="$vcpkg_root/scripts/buildsystems/vcpkg.cmake"

manifest_cmake_args=(
  -S "$duckdb_source_dir"
  -B "$manifest_build_dir"
  -G Ninja
  -DCMAKE_BUILD_TYPE=Debug
  -DCMAKE_TOOLCHAIN_FILE="$toolchain_file"
  -DVCPKG_BUILD=1
  -DVCPKG_TARGET_TRIPLET="$vcpkg_triplet"
  -DVCPKG_MANIFEST_MODE=OFF
  -DDUCKDB_EXPLICIT_PLATFORM="$duckdb_platform"
  -DDUCKDB_EXTENSION_CONFIGS="$extension_config"
  -DEXTENSION_CONFIG_BUILD=TRUE
)
cmake "${manifest_cmake_args[@]}"
cmake --build "$manifest_build_dir" --target duckdb_merge_vcpkg_manifests --parallel
test -s "$generated_manifest_dir/vcpkg.json"
quack_source_dir="$manifest_build_dir/_deps/quack_extension_fc-src"
if [[ "$(git -C "$quack_source_dir" rev-parse HEAD)" != "$QUACK_REVISION" ]]; then
  echo "Quack checkout did not resolve to $QUACK_REVISION" >&2
  exit 1
fi
httpfs_source_dir="$manifest_build_dir/_deps/httpfs_extension_fc-src"
if [[ "$(git -C "$httpfs_source_dir" rev-parse HEAD)" != "$HTTPFS_REVISION" ]]; then
  echo "httpfs checkout did not resolve to $HTTPFS_REVISION" >&2
  exit 1
fi

cmake_args=(
  -S "$duckdb_source_dir"
  -B "$build_dir"
  -G Ninja
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_TOOLCHAIN_FILE="$toolchain_file"
  -DVCPKG_BUILD=1
  -DVCPKG_TARGET_TRIPLET="$vcpkg_triplet"
  -DVCPKG_MANIFEST_DIR="$generated_manifest_dir"
  -DVCPKG_INSTALLED_DIR="$vcpkg_installed_dir"
  -DDUCKDB_EXPLICIT_PLATFORM="$duckdb_platform"
  -DDUCKDB_EXTENSION_CONFIGS="$extension_config"
  -DOVERRIDE_GIT_DESCRIBE="v1.5.4-0-g08e34c447b"
  -DBUILD_UNITTESTS=OFF
  -DBUILD_BENCHMARKS=OFF
  -DBUILD_SHELL=ON
  -DENABLE_EXTENSION_AUTOLOADING=OFF
  -DENABLE_EXTENSION_AUTOINSTALL=OFF
)
if [[ "$(uname -s)" == "Darwin" ]]; then
  cmake_args+=("-DCMAKE_OSX_ARCHITECTURES=$(uname -m)")
fi

cmake "${cmake_args[@]}"
if [[ "$(git -C "$build_dir/_deps/quack_extension_fc-src" rev-parse HEAD)" != "$QUACK_REVISION" ]]; then
  echo "Release Quack checkout did not resolve to $QUACK_REVISION" >&2
  exit 1
fi
if [[ "$(git -C "$build_dir/_deps/httpfs_extension_fc-src" rev-parse HEAD)" != "$HTTPFS_REVISION" ]]; then
  echo "Release httpfs checkout did not resolve to $HTTPFS_REVISION" >&2
  exit 1
fi
cmake --build "$build_dir" --target duckdb httpfs_loadable_extension quack_loadable_extension sqlnb_loadable_extension --parallel

duckdb_binary="$build_dir/duckdb"
quack_artifact="$build_dir/extension/quack/quack.duckdb_extension"
httpfs_artifact="$build_dir/extension/httpfs/httpfs.duckdb_extension"
sidequery_artifact="$build_dir/extension/sqlnb/sqlnb.duckdb_extension"
test -x "$duckdb_binary"
test -s "$quack_artifact"
test -s "$httpfs_artifact"
test -s "$sidequery_artifact"

bun "$extension_dir/scripts/runtime-smoke.mjs" --duckdb "$duckdb_binary" --extension "$sidequery_artifact" --quack-extension "$quack_artifact" --httpfs-extension "$httpfs_artifact"

mkdir -p "$artifact_parent"
artifact_stage="$(mktemp -d "$artifact_parent/.${duckdb_platform}.new.XXXXXX")"
cp "$duckdb_binary" "$artifact_stage/duckdb"
cp "$quack_artifact" "$artifact_stage/quack.duckdb_extension"
cp "$httpfs_artifact" "$artifact_stage/httpfs.duckdb_extension"
cp "$sidequery_artifact" "$artifact_stage/sqlnb.duckdb_extension"
{
  echo "duckdb_version=$DUCKDB_VERSION"
  echo "duckdb_revision=$DUCKDB_REVISION"
  echo "quack_revision=$QUACK_REVISION"
  echo "httpfs_revision=$HTTPFS_REVISION"
  echo "vcpkg_revision=$VCPKG_REVISION"
  echo "duckdb_platform=$duckdb_platform"
} >"$artifact_stage/manifest.txt"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$artifact_stage" && sha256sum duckdb httpfs.duckdb_extension quack.duckdb_extension sqlnb.duckdb_extension >SHA256SUMS)
else
  (cd "$artifact_stage" && shasum -a 256 duckdb httpfs.duckdb_extension quack.duckdb_extension sqlnb.duckdb_extension >SHA256SUMS)
fi

# Rename complete directories so a running CLI never observes a partially
# replaced executable or extension library.
artifact_previous="$artifact_parent/.${duckdb_platform}.previous.$$"
if [[ -e "$artifact_dir" ]]; then
  mv "$artifact_dir" "$artifact_previous"
fi
mv "$artifact_stage" "$artifact_dir"
if [[ -e "$artifact_previous" ]]; then
  rm -rf -- "$artifact_previous"
fi

echo "Retained runnable DuckDB, Quack, and SQLNB artifacts in $artifact_dir"
