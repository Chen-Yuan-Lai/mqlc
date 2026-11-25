#!/usr/bin/env bash
# Installs the official MongoDB MCP server at the version pinned in
# tools/mongodb-mcp/package-lock.json, for experiment E (ADR 0015).
set -euo pipefail
cd "$(dirname "$0")/../tools/mongodb-mcp"
if [[ -f package-lock.json ]]; then
  npm ci --ignore-scripts --no-audit --no-fund
else
  npm install --ignore-scripts --no-audit --no-fund
fi
echo "installed $(node -p 'require("./node_modules/mongodb-mcp-server/package.json").version') in $PWD"
