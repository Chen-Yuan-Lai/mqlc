#!/usr/bin/env bash
# Download the AptMQL-Bench dataset (questions, prompts, dumps, ~2.6 GB) into
# data/AptMQL-Bench. Safe to re-run: files already present are skipped.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
uvx --from huggingface_hub hf download MongoDB/AptMQL-Bench \
  --repo-type dataset --local-dir data/AptMQL-Bench
