#!/usr/bin/env bash
set -euo pipefail
umask 077

DEPLOY_ROOT="$HOME/$1"
export DEPLOY_RELEASE="$2"
export DEPLOY_REVISION="$3"
RELEASE_DIR="$DEPLOY_ROOT/releases/$DEPLOY_RELEASE"
mkdir -p "$DEPLOY_ROOT/shared" "$DEPLOY_ROOT/backups"
exec 9>"$DEPLOY_ROOT/shared/deploy.lock"
flock -n 9 || { echo 'Another deployment is running.' >&2; exit 1; }

env_file="$DEPLOY_ROOT/shared/.env.remote"
if [[ ! -f "$env_file" ]]; then
    printf 'POSTGRES_PASSWORD=%s\nMATOMO_IMAGE_TAG=5.10.1-apache\nMATOMO_PORT=18084\nMATOMO_ARCHIVE_URL=http://matomo\n' \
        "$(openssl rand -hex 32)" >"$env_file"
    chmod 600 "$env_file"
fi

cd "$RELEASE_DIR"
compose=(docker compose --project-name matomo-mysql2pg --env-file "$env_file" -f compose.remote.yml)
"${compose[@]}" config --quiet
# Keep the existing stack running until the new middleware image is built.
"${compose[@]}" build middleware
"${compose[@]}" pull postgres matomo archive

if [[ -L "$DEPLOY_ROOT/current" ]]; then
    backup_dir="$DEPLOY_ROOT/backups/$DEPLOY_RELEASE"
    mkdir -p "$backup_dir"
    "${compose[@]}" exec -T postgres pg_dump -U matomo -d matomo -Fc >"$backup_dir/postgres.dump"
    "${compose[@]}" exec -T matomo tar -C /var/www/html -czf - config >"$backup_dir/matomo-config.tar.gz"
    cp "$env_file" "$backup_dir/env.remote"
    printf 'Pre-deploy backup saved in %s\n' "$backup_dir"
fi

if ! "${compose[@]}" up -d --wait --wait-timeout 360; then
    "${compose[@]}" ps -a
    "${compose[@]}" logs --no-color --tail=80 middleware matomo
    echo 'Deployment failed. Existing volumes and release directories were retained.' >&2
    exit 1
fi

"${compose[@]}" exec -T matomo php /opt/deployment/healthcheck.php
"${compose[@]}" exec -T matomo php -r '
    require "/var/www/html/core/Version.php";
    echo "Matomo version: ", \Piwik\Version::VERSION, PHP_EOL;
'
endpoint="$("${compose[@]}" port matomo 80)"
curl --fail --silent --show-error --max-time 30 "http://$endpoint/" -o /dev/null
if "${compose[@]}" exec -T matomo php /opt/deployment/installed.php; then
    echo 'Matomo configuration exists. Check application login.'
else
    echo 'Complete the Matomo installation wizard through the SSH tunnel.'
fi
ln -sfn "releases/$DEPLOY_RELEASE" "$DEPLOY_ROOT/current"
printf '%s\n' "$DEPLOY_REVISION" >"$DEPLOY_ROOT/shared/deployed-revision"
"${compose[@]}" ps
printf 'Deployment ready in %s/current\n' "$DEPLOY_ROOT"
