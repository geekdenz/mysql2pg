#!/usr/bin/env bash
set -euo pipefail

host="${DB_HOST:-middleware}"
port="${DB_PORT:-3306}"
user="${DB_USERNAME:-anyuser}"
pass="${DB_PASSWORD:-matomo}"
name="${DB_DATABASE:-bookstack}"

echo "[bookstack] waiting for ${host}:${port} ..."
for _ in $(seq 1 60); do
    # --skip-ssl: the middleware speaks plaintext MySQL, while the Debian
    # MariaDB client now demands TLS by default.
    if mysql --skip-ssl -h "$host" -P "$port" -u "$user" -p"$pass" \
            -e 'SELECT 1' >/dev/null 2>&1; then
        echo "[bookstack] database reachable"
        break
    fi
    sleep 2
done

# The middleware maps a MySQL database onto a PostgreSQL schema and creates it
# on CREATE DATABASE, so the example provisions its own.
mysql --skip-ssl -h "$host" -P "$port" -u "$user" -p"$pass" \
    -e "CREATE DATABASE IF NOT EXISTS \`${name}\`" >/dev/null 2>&1 || true

# BookStack refuses to boot without an application key.
if [ ! -f /var/www/html/.env ]; then
    printf 'APP_KEY=\nAPP_URL=%s\n' "${APP_URL:-http://localhost:8084}" > /var/www/html/.env
    php artisan key:generate --force --no-interaction >/dev/null
fi

if [ "${BOOKSTACK_SKIP_MIGRATE:-0}" != "1" ]; then
    echo "[bookstack] running migrations ..."
    php artisan migrate --force --no-interaction || {
        echo "[bookstack] migrate FAILED" >&2
        exit 1
    }
fi

chown -R www-data:www-data /var/www/html/storage /var/www/html/bootstrap/cache 2>/dev/null || true

exec "$@"
