#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITE_DIR="${ROOT_DIR}/mysql-test/suite/mysql2pg"
TEST_BIN="${MARIADB_TEST_BIN:-mariadb-test}"
HOST="${MYSQL_TEST_HOST:-127.0.0.1}"
PORT="${MYSQL_TEST_PORT:-3306}"
USER_NAME="${MYSQL_TEST_USER:-anyuser}"
PASSWORD="${MYSQL_TEST_PASSWORD:-}"
DATABASE_NAME="${MYSQL_TEST_DATABASE:-app}"
LOG_DIR="${MYSQL_TEST_LOGDIR:-${ROOT_DIR}/tmp/mariadb-test-logs}"
MODE="verify"

usage() {
  cat <<'EOF'
Usage: scripts/run-mariadb-suite.sh [--record] [test-name ...]

Runs the repo-local MariaDB/MySQL compatibility suite against the middleware.

Environment overrides:
  MARIADB_TEST_BIN     mariadb-test executable path
  MYSQL_TEST_HOST      server host (default 127.0.0.1)
  MYSQL_TEST_PORT      server port (default 3306)
  MYSQL_TEST_USER      login user (default anyuser)
  MYSQL_TEST_PASSWORD  login password (default empty)
  MYSQL_TEST_DATABASE  default database (default app)
  MYSQL_TEST_LOGDIR    log directory

Examples:
  scripts/run-mariadb-suite.sh
  scripts/run-mariadb-suite.sh --record
  scripts/run-mariadb-suite.sh text_smoke metadata
EOF
}

declare -a FILTERS=()
for arg in "$@"; do
  case "${arg}" in
    --record)
      MODE="record"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      FILTERS+=("${arg}")
      ;;
  esac
done

mkdir -p "${LOG_DIR}"
mkdir -p "${SUITE_DIR}/r"

matches_filters() {
  local base="$1"
  if [[ "${#FILTERS[@]}" -eq 0 ]]; then
    return 0
  fi

  local filter
  for filter in "${FILTERS[@]}"; do
    if [[ "${base}" == "${filter}" ]]; then
      return 0
    fi
  done
  return 1
}

run_test() {
  local test_file="$1"
  local base_name="${test_file##*/}"
  base_name="${base_name%.test}"

  if ! matches_filters "${base_name}"; then
    return 0
  fi

  local result_file="${SUITE_DIR}/r/${base_name}.result"
  local timer_file="${LOG_DIR}/${base_name}.timer"
  local progress_file="${LOG_DIR}/${base_name}.progress"
  local mode_flags=()

  if [[ "${MODE}" == "record" ]]; then
    mode_flags+=(--record)
  fi

  echo "==> ${base_name}"
  "${TEST_BIN}" \
    --no-defaults \
    --host "${HOST}" \
    --port "${PORT}" \
    --protocol tcp \
    --user "${USER_NAME}" \
    --database "${DATABASE_NAME}" \
    --silent \
    --skip-ssl \
    --max-connect-retries 20 \
    --tail-lines 40 \
    --timer-file "${timer_file}" \
    --logdir "${LOG_DIR}" \
    --test-file "${test_file}" \
    --result-file "${result_file}" \
    "${mode_flags[@]}" \
    ${PASSWORD:+--password="${PASSWORD}"}

  rm -f "${progress_file}" "${timer_file}"
}

shopt -s nullglob
tests=("${SUITE_DIR}/t/"*.test)
if [[ "${#tests[@]}" -eq 0 ]]; then
  echo "No tests found in ${SUITE_DIR}/t" >&2
  exit 1
fi

for test_file in "${tests[@]}"; do
  run_test "${test_file}"
done
