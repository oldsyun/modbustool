#!/usr/bin/env bash
# Build helper for the GPUI app. Handles the quirks of this toolchain:
#  - CARGO_HOME redirected into the workspace (sandbox blocks ~/.cargo writes)
#  - RUSTC_BOOTSTRAP=1 + vendored gpui-pre-macos (pinned gpui-pre 0.3.11 uses an
#    unstable stdlib feature `result_option_map_or_default`; we patch the crate)
# Usage: ./scripts/cargo-gpui.sh build|run|check [--release]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$ROOT/.cargo-home"
export CARGO_HOME="$ROOT/.cargo-home"
export CARGO_TARGET_DIR="$ROOT/target-gpui"
export RUSTC_BOOTSTRAP=1
cmd="${1:-build}"; shift || true
exec cargo "$cmd" -p modbus-tool-gpui "$@"
