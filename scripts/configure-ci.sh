#!/usr/bin/env bash
set -euo pipefail

# Linux setup runs once on the host and again inside the build container.
# Install the cache in the environment that actually compiles Rust.
if [[ "${LINUX_CI_IN_DOCKER:-}" == 0 ]]; then
  exit 0
fi
if [[ -n "${RUSTC_WRAPPER:-}" || -n "${RUSTC_WORKSPACE_WRAPPER:-}" ]]; then
  echo "Using the configured Rust compiler wrapper"
  exit 0
fi
if command -v mbx >/dev/null 2>&1; then
  exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${OS:-}" == Windows_NT ]]; then
  # Community setup precedes the build phase that normally activates MSVC.
  powershell -NoProfile -File "$repo_root/scripts/configure-ci-windows.ps1"
  exit 0
fi
cargo install mbx --version 1.27.0 --locked --root "$repo_root/.ci/tools"
