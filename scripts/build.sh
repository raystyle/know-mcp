#!/usr/bin/env bash
# One-shot build wrapper: stable env + debug/release switch.
set -e
cd "$(dirname "$0")/.."
source scripts/env.sh
case "${1:-debug}" in
  debug) cargo build ;;
  release) cargo build --release ;;
  lint) cargo dylint --all ;;
  *) echo "usage: build.sh [debug|release|lint]" >&2; exit 2 ;;
esac
