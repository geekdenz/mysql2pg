#!/usr/bin/env bash
set -euo pipefail

host="${SS_DATABASE_SERVER:-middleware}"
port="${SS_DATABASE_PORT:-3306}"
user="${SS_DATABASE_USERNAME:-app}"
pass="${SS_DATABASE_PASSWORD:-app}"
name="${SS_DATABASE_NAME:-app}"

echo "[silverstripe] waiting for ${host}:${port} ..."
for _ in $(seq 1 60); do
    # --skip-ssl: the middleware speaks plaintext MySQL, while the Debian
    # MariaDB client now demands TLS by default.
    if mysql --skip-ssl -h "$host" -P "$port" -u "$user" -p"$pass" "$name" \
            -e 'SELECT 1' >/dev/null 2>&1; then
        echo "[silverstripe] database reachable"
        break
    fi
    sleep 2
done

# The middleware maps a MySQL database onto a PostgreSQL schema, and creates it
# on CREATE DATABASE. Doing this here keeps the example self-provisioning.
mysql --skip-ssl -h "$host" -P "$port" -u "$user" -p"$pass" \
    -e "CREATE DATABASE IF NOT EXISTS \`${name}\`" >/dev/null 2>&1 || true

# dev/build creates and migrates the schema; this is the step that exercises the
# middleware hardest, because SilverStripe introspects every table it manages.
if [ "${SS_SKIP_DEV_BUILD:-0}" != "1" ]; then
    echo "[silverstripe] running dev/build ..."
    vendor/bin/sake dev/build flush=1 || {
        echo "[silverstripe] dev/build FAILED" >&2
        exit 1
    }
fi

chown -R www-data:www-data /var/www/html/public/assets /var/www/html/silverstripe-cache 2>/dev/null || true

exec "$@"
