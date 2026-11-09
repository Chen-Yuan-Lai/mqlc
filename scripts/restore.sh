#!/usr/bin/env bash
# Start MongoDB and restore all 21 AptMQL-Bench databases with the dataset's
# own restore.sh, run inside the container (no host MongoDB tools needed).
# Upstream ships restore.sh with CRLF line endings, so strip them first.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
[ -f data/AptMQL-Bench/restore.sh ] || scripts/fetch-data.sh
docker compose up -d --wait mongo
docker compose exec -T \
  -e DATA_ROOT=/bench \
  -e MONGO_URI="mongodb://root:root@localhost:27017/?authSource=admin" \
  mongo bash -c "tr -d '\\r' < /bench/restore.sh | bash -s"
