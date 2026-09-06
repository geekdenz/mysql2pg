#!/usr/bin/env bash
set -euo pipefail
umask 077

deploy_dir="${1:?Deployment directory is required}"
[[ "$deploy_dir" =~ ^[a-zA-Z0-9_-]+(/[a-zA-Z0-9_-]+)*$ ]] || {
    echo 'Invalid deployment directory.' >&2
    exit 2
}

deploy_root="$HOME/$deploy_dir"
postgres_container=matomo-mysql2pg-postgres-1
matomo_container=matomo-mysql2pg-matomo-1
archive_container=matomo-mysql2pg-archive-1
matomo_volume=matomo-mysql2pg_matomo_data

for container in "$postgres_container" "$matomo_container" "$archive_container"; do
    docker inspect "$container" >/dev/null
done
docker volume inspect "$matomo_volume" >/dev/null

reset_id="manual-reset-$(date -u +%Y%m%dT%H%M%SZ)"
backup_dir="$deploy_root/backups/$reset_id"
mkdir -p "$backup_dir"
chmod 700 "$backup_dir"

docker exec "$postgres_container" pg_dump -U matomo -d matomo -Fc \
    >"$backup_dir/postgres.dump"
docker exec "$matomo_container" tar -C /var/www/html -czf - config \
    >"$backup_dir/matomo-config.tar.gz"

restart_services() {
    docker start "$matomo_container" "$archive_container" >/dev/null 2>&1 || true
}
trap restart_services EXIT
docker stop "$archive_container" "$matomo_container" >/dev/null

docker exec "$postgres_container" psql -v ON_ERROR_STOP=1 -U matomo -d matomo \
    -c 'DROP SCHEMA public CASCADE; CREATE SCHEMA public AUTHORIZATION matomo;'
docker run --rm -v "$matomo_volume:/var/www/html" alpine:3.22 sh -c '
    rm -f /var/www/html/config/config.ini.php
    find /var/www/html/tmp/sessions -mindepth 1 -maxdepth 1 -type f -delete 2>/dev/null || true
    find /var/www/html/tmp/cache -mindepth 1 -maxdepth 1 -type d -exec rm -rf -- {} + 2>/dev/null || true
'

restart_services
trap - EXIT

for _ in $(seq 1 60); do
    status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$matomo_container")"
    [[ "$status" == healthy ]] && break
    sleep 2
done
[[ "${status:-}" == healthy ]] || {
    docker logs --tail=80 "$matomo_container" >&2
    echo 'Matomo did not become healthy after the reset.' >&2
    exit 1
}

table_count="$(docker exec "$postgres_container" psql -U matomo -d matomo -Atc \
    "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")"
[[ "$table_count" == 0 ]] || {
    echo "Expected an empty Matomo schema, found $table_count tables." >&2
    exit 1
}
docker exec "$matomo_container" test ! -f /var/www/html/config/config.ini.php

printf 'Matomo reset complete. Recovery backup: %s\n' "$backup_dir"
printf 'The installation wizard is ready.\n'
