#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}/e2e"

npm ci --no-audit --no-fund
npx playwright install --with-deps chromium
npm test
