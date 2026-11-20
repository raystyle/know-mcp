#!/usr/bin/env bash
# know-mcp-server build & lint workflow (three tiers).
#   rustc + clippy : cargo lint            (fast, everyday)
set -e
cd "$(dirname "$0")/.."
source scripts/env.sh

case "${1:-build}" in
  build)
    cargo build "${@:2}"
    ;;
  release)
    cargo build --release
    ;;
  lint)      # clippy + project bans (clippy.toml disallowed-methods)
    cargo clippy --all-targets -- -D warnings --no-deps
    ;;
  test)
    cargo test -p know-core
    ;;
  *)
    echo "usage: build.sh [build|release|lint|test]" >&2
    exit 2
    ;;
esac
