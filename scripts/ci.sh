#!/usr/bin/env bash
# The checks CI runs; run it before every commit. Docker must be running: the
# integration tests and the scorer test start mongo:8.2 through testcontainers.
#   scripts/ci.sh            everything
#   scripts/ci.sh rust       the `test` job
#   scripts/ci.sh scoring    the `scoring` job
#   scripts/ci.sh quick      fmt and clippy only (the pre-commit hook)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

lint() {
  cargo fmt --all --check
  cargo clippy --all-targets -- -D warnings
}

rust() {
  scripts/fetch-data.sh aptmqlbench_data.jsonl prompts/s51_text2mql_1time_completion.txt
  lint
  cargo test --all-targets
}

scoring() {
  (cd scoring && uv run pytest -q)
}

case "${1:-all}" in
  all) rust; scoring ;;
  rust) rust ;;
  scoring) scoring ;;
  quick) lint ;;
  *) echo "usage: scripts/ci.sh [all|rust|scoring|quick]" >&2; exit 2 ;;
esac
