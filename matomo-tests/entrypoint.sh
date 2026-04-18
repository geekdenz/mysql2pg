#!/usr/bin/env bash

set -euo pipefail

MATOMO_ROOT="${MATOMO_ROOT:-/var/www/html}"

mkdir -p \
  "${MATOMO_ROOT}/tmp/cache/tracker" \
  "${MATOMO_ROOT}/tmp/cache/archive" \
  "${MATOMO_ROOT}/tmp/cache/template" \
  "${MATOMO_ROOT}/tmp/assets"

chown -R www-data:www-data "${MATOMO_ROOT}/tmp"
chmod -R u+rwX,g+rwX "${MATOMO_ROOT}/tmp"

exec /entrypoint.sh "$@"
