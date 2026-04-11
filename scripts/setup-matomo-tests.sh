#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.matomo-tests.yml"
MATOMO_DIR="${ROOT_DIR}/tmp/matomo-5.8.0"

if [[ ! -d "${MATOMO_DIR}" ]]; then
  echo "Missing ${MATOMO_DIR}. Clone Matomo 5.8.0 first." >&2
  exit 1
fi

docker compose -f "${COMPOSE_FILE}" up --build -d

docker compose -f "${COMPOSE_FILE}" exec -T matomo-tests-postgres psql \
  -U postgres \
  -d app \
  -c "DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;"

docker compose -f "${COMPOSE_FILE}" exec -T matomo-tests-web bash -lc \
  "composer install --no-interaction --prefer-dist"

docker compose -f "${COMPOSE_FILE}" exec -T matomo-tests-web bash -lc \
  "php tests/resources/install-matomo.php . '{\"host\":\"matomo-tests-middleware\",\"username\":\"anyuser\",\"password\":\"matomo\",\"port\":3306,\"adapter\":\"MYSQLI\",\"type\":\"InnoDB\",\"schema\":\"Mysql\",\"charset\":\"utf8mb4\",\"collation\":\"utf8mb4_general_ci\"}' matomo-tests-web"

docker compose -f "${COMPOSE_FILE}" exec -T matomo-tests-web php /opt/mysql2pg/scripts/update-matomo-test-config.php
