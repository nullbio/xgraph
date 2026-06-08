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
#   ./install.sh                 # install latest master
#   ./install.sh --skills        # install latest master and update global skills
#   ./install.sh --tag v0.2.0    # install a tagged release
#   ./install.sh --force         # reinstall even if version unchanged
#
# Any extra args are forwarded to `cargo install`.

set -euo pipefail

readonly REPO="https://github.com/nullbio/xgraph"
readonly NEEDED_CXX_FLAG="-include cstdint"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SKILL_NAME="xgraph"
readonly LOCAL_SKILL_DIR="$SCRIPT_DIR/skills/$SKILL_NAME"
readonly AGENTS_SKILLS_DIR="${AGENTS_SKILLS_DIR:-$HOME/.agents/skills}"
readonly CLAUDE_SKILLS_DIR="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"

print_skills_notice() {
    cat <<'EOF'
Note: use --skills to also install or update the globally installed xgraph skill.
Before running with --skills, diff the installed skill against this repository's
skills/xgraph copy. The --skills flag overwrites ~/.agents/skills/xgraph and
refreshes ~/.claude/skills/xgraph, so do not use it when the installed skill has
local changes you may want to keep. Review those differences with the user first.

EOF
}

print_usage() {
    cat <<'EOF'
Usage:
  ./install.sh [--skills] [cargo install options...]

Options:
  --skills      Also install/update the global xgraph skill.
  -h, --help    Show this help.

All other arguments are forwarded to:
  cargo install --git https://github.com/nullbio/xgraph --force

Examples:
  ./install.sh
  ./install.sh --skills
  ./install.sh --tag v0.2.0
EOF
}

install_skills() {
    if [[ ! -f "$LOCAL_SKILL_DIR/SKILL.md" ]]; then
        echo "error: xgraph skill not found at $LOCAL_SKILL_DIR" >&2
        exit 1
    fi

    mkdir -p "$AGENTS_SKILLS_DIR" "$CLAUDE_SKILLS_DIR"
    rm -rf "$AGENTS_SKILLS_DIR/$SKILL_NAME"
    cp -a "$LOCAL_SKILL_DIR" "$AGENTS_SKILLS_DIR/$SKILL_NAME"

    rm -rf "$CLAUDE_SKILLS_DIR/$SKILL_NAME"
    ln -s "$AGENTS_SKILLS_DIR/$SKILL_NAME" "$CLAUDE_SKILLS_DIR/$SKILL_NAME"

    echo "==> Installed xgraph skill to $AGENTS_SKILLS_DIR/$SKILL_NAME"
    echo "==> Linked Claude skill at $CLAUDE_SKILLS_DIR/$SKILL_NAME"
    echo
}

install_skill=false
cargo_args=()
for arg in "$@"; do
    case "$arg" in
        --skills)
            install_skill=true
            ;;
        -h|--help)
            print_skills_notice
            print_usage
            exit 0
            ;;
        *)
            cargo_args+=("$arg")
            ;;
    esac
done

print_skills_notice

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

if [[ "$install_skill" == true ]]; then
    install_skills
fi

echo "==> Installing xgraph from $REPO"
echo "    CXXFLAGS=\"$merged_cxxflags\""
echo

# Default to --force so re-runs pick up new commits without the user
# having to remember the flag. The user can still pass extra cargo
# install args via "$@".
exec env CXXFLAGS="$merged_cxxflags" cargo install --git "$REPO" --force "${cargo_args[@]}"
