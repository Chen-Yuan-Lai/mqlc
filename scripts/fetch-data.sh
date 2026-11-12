#!/usr/bin/env bash
# Download the AptMQL-Bench dataset (questions, prompts, dumps, ~2.6 GB) into
# data/AptMQL-Bench. Safe to re-run: files already present are skipped.
# File names as arguments fetch only those, e.g. CI's
#   scripts/fetch-data.sh aptmqlbench_data.jsonl prompts/s51_text2mql_1time_completion.txt
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
# Pinned so scores stay comparable; bump deliberately.
REVISION=ac56b3454184b77b032be0b1866a489427dada05
uvx --from huggingface_hub hf download MongoDB/AptMQL-Bench "$@" \
  --repo-type dataset --revision "$REVISION" --local-dir data/AptMQL-Bench
