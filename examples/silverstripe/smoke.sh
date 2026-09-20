#!/usr/bin/env bash
# Proves SilverStripe runs on PostgreSQL through the middleware.
#
#   docker compose --profile silverstripe up -d --build
#   ./examples/silverstripe/smoke.sh
#
# The decisive checks are the last ones: rows written through SilverStripe's
# MariaDB/PDO driver are read straight back out of PostgreSQL with psql.
#
# The example lives in its own "silverstripe" PostgreSQL schema, which this
# script drops first so the run is repeatable and independent of whatever else
# the local stack has in it.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

SCHEMA="${SS_DATABASE_NAME:-silverstripe}"

failures=0
check() { # name expected actual
    if [[ "$3" == *"$2"* ]]; then printf 'ok   %s\n' "$1"
    else printf 'FAIL %s\n     expected to contain: %s\n     got: %s\n' "$1" "$2" "$3"; failures=$((failures+1)); fi
}
ss() { docker compose --profile silverstripe run --rm -T -e SS_SKIP_DEV_BUILD=1 silverstripe "$@" 2>&1; }
# Queries are schema-qualified rather than relying on search_path, so a missing
# schema shows up as a clear failure instead of silently reading public.
pg() { docker compose exec -T postgres psql -U postgres -d app -tAc "$1" 2>&1; }

echo "== resetting the example schema =="
docker compose exec -T postgres psql -U postgres -d app -q \
    -c "DROP SCHEMA IF EXISTS $SCHEMA CASCADE" >/dev/null 2>&1

echo
echo "== schema build =="
build_out="$(ss vendor/bin/sake dev/build flush=1)"
check "dev/build completes"                   "Database build completed" "$build_out"
check "dev/build creates the CMS tables"      "Table SiteTree: created"  "$build_out"
check "dev/build creates the example table"   "Table ProofRecord: created" "$build_out"

echo
echo "== the driver really is MariaDB over PDO =="
driver_out="$(ss vendor/bin/sake dev/tasks/mysql2pg-driver-report flush=1)"
check "connector is PDO"                      "connector=PDOConnector" "$driver_out"
check "server reports as MariaDB"             "MariaDB"                "$driver_out"
check "ANSI quoting is in effect"             "ansi=yes"               "$driver_out"

echo
echo "== ORM write / read / aggregate =="
proof_out="$(ss vendor/bin/sake dev/tasks/mysql2pg-proof flush=1)"
check "ORM round-trip"                        "total=3 active=2" "$proof_out"
check "aggregate SUM"                         "sum_qty=20"       "$proof_out"
check "aggregate MAX"                         "max_price=101.25" "$proof_out"
check "ORDER BY picks the right row"          "top=Gadget"       "$proof_out"
check "partial-match filter"                  "partial=Gadget"   "$proof_out"
check "UPDATE is visible on re-read"          "updated=99"       "$proof_out"

echo
echo "== the data is genuinely in PostgreSQL =="
check "CMS tables exist in PostgreSQL"        "SiteTree" \
      "$(pg "SELECT tablename FROM pg_tables WHERE schemaname = '$SCHEMA' AND tablename = 'SiteTree'")"
check "row count matches"                     "3"      "$(pg "SELECT count(*) FROM $SCHEMA.\"ProofRecord\"")"
check "updated value persisted"               "99"     "$(pg "SELECT \"Quantity\" FROM $SCHEMA.\"ProofRecord\" WHERE \"Title\" = 'Gadget'")"
check "decimal survived the round trip"       "101.25" "$(pg "SELECT \"Price\" FROM $SCHEMA.\"ProofRecord\" WHERE \"Title\" = 'Doohickey'")"
check "the default home page was written"     "1"      "$(pg "SELECT count(*) FROM $SCHEMA.\"SiteTree\" WHERE \"URLSegment\" = 'home'")"

echo
if (( failures )); then echo "$failures check(s) failed"; exit 1; fi
echo "all checks passed"
