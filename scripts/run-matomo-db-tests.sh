#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.matomo-tests.yml"

docker compose -f "${COMPOSE_FILE}" exec -T \
  -e MYSQL_ADAPTER=MYSQLI \
  -e MYSQL_ENGINE=Mysql \
  matomo-tests-web \
  bash -lc "cd /var/www/html/tests/PHPUnit && php ../../vendor/phpunit/phpunit/phpunit --configuration phpunit.xml.dist ../PHPUnit/Integration/Db/BatchInsertTest.php ../PHPUnit/Integration/Db/TransactionLevelTest.php ../PHPUnit/Integration/Tracker/DbTest.php ../PHPUnit/Integration/Tracker/Db/MysqliTest.php"
