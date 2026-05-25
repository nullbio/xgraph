#!/usr/bin/env bash
# xgraph install / upgrade helper.
#
# Why this exists: cozo 0.7 vendors a RocksDB whose C++ headers omit
# `#include <cstdint>`. On GCC 13+ this no longer comes in transitively,
# so the build fails with hundreds of "uint64_t does not name a type"
# errors. Setting CXXFLAGS="-include cstdint" forces the header in
# everywhere and makes the build succeed.
#
# Usage:
#   ./install.sh               # install latest master
#   ./install.sh --tag v0.2.0  # install a tagged release
#   ./install.sh --force       # reinstall even if version unchanged
#
# Any extra args are forwarded to `cargo install`.

set -euo pipefail

readonly REPO="https://github.com/nullbio/xgraph"
readonly NEEDED_CXX_FLAG="-include cstdint"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required (install Rust: https://rustup.rs/)" >&2
    exit 1
fi

# Preserve any user-supplied CXXFLAGS, append our forced include if it's
# not already present. The `-include` flag injects a header at the top
# of every translation unit so RocksDB's missing-<cstdint> bug stops
# breaking the build on modern GCC.
existing_cxxflags="${CXXFLAGS:-}"
case " $existing_cxxflags " in
    *" $NEEDED_CXX_FLAG "*)
        merged_cxxflags="$existing_cxxflags"
        ;;
    *)
        merged_cxxflags="${existing_cxxflags:+$existing_cxxflags }${NEEDED_CXX_FLAG}"
        ;;
esac

echo "==> Installing xgraph from $REPO"
echo "    CXXFLAGS=\"$merged_cxxflags\""
echo

# Default to --force so re-runs pick up new commits without the user
# having to remember the flag. The user can still pass extra cargo
# install args via "$@".
exec env CXXFLAGS="$merged_cxxflags" cargo install --git "$REPO" --force "$@"
