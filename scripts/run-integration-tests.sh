#!/usr/bin/env bash

set -euo pipefail

HOST="${MYSQL_TEST_HOST:-127.0.0.1}"
PORT="${MYSQL_TEST_PORT:-3306}"
USER_NAME="${MYSQL_TEST_USER:-anyuser}"
PASSWORD="${MYSQL_TEST_PASSWORD:-}"
DATABASE_NAME="${MYSQL_TEST_DATABASE:-app}"

mysql_args=(--host "${HOST}" --port "${PORT}" --user "${USER_NAME}" --database "${DATABASE_NAME}" --batch --skip-column-names)
if [[ -n "${PASSWORD}" ]]; then
  mysql_args+=("--password=${PASSWORD}")
fi

run_sql() {
  mariadb "${mysql_args[@]}" --execute "$1"
}

echo "Checking temporary-table lifetime on one MySQL connection..."
temporary_result="$(run_sql "DROP TEMPORARY TABLE IF EXISTS mysql2pg_integration_temp; CREATE TEMPORARY TABLE mysql2pg_integration_temp (value INT); INSERT INTO mysql2pg_integration_temp VALUES (7), (9); SELECT SUM(value) FROM mysql2pg_integration_temp;")"
[[ "${temporary_result}" == "16" ]] || {
  echo "temporary-table check failed: ${temporary_result}" >&2
  exit 1
}

echo "Checking ALTER TABLE column and index translation..."
alter_result="$(run_sql "DROP TABLE IF EXISTS mysql2pg_integration_alter; CREATE TABLE mysql2pg_integration_alter (id INT); ALTER TABLE mysql2pg_integration_alter ADD COLUMN value INT UNSIGNED NULL, ADD INDEX index_value (value); SHOW INDEX FROM mysql2pg_integration_alter;")"
grep -q "index_value" <<<"${alter_result}" || {
  echo "ALTER TABLE index check failed: ${alter_result}" >&2
  exit 1
}
run_sql "DROP TABLE mysql2pg_integration_alter;" >/dev/null

echo "Integration checks passed."
