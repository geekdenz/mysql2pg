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

if [[ -n "${MATOMO_INTERNAL_PORT:-}" && "${MATOMO_INTERNAL_PORT}" != "80" ]]; then
  sed -i -E "s/Listen 80/Listen ${MATOMO_INTERNAL_PORT}/; s/<VirtualHost \*:80>/<VirtualHost *:${MATOMO_INTERNAL_PORT}>/" \
    /etc/apache2/ports.conf /etc/apache2/sites-available/000-default.conf
fi

exec /entrypoint.sh "$@"
