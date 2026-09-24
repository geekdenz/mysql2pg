#!/usr/bin/env bash
# Proves SilverStripe runs on PostgreSQL through the middleware, over both of
# the MySQL client libraries PHP ships: PDO (pdo_mysql) and mysqli.
#
#   docker compose --profile silverstripe up -d --build
#   ./examples/silverstripe/smoke.sh            # both connectors
#   ./examples/silverstripe/smoke.sh mysqli     # just one (pdo|mysqli|probe)
#
# Each connector gets its own PostgreSQL schema, dropped first, so the runs are
# independent and repeatable. The decisive checks are the last ones in each
# block: rows written through SilverStripe are read back out with psql.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

failures=0
check() { # name expected actual
    if [[ "$3" == *"$2"* ]]; then printf '  ok   %s\n' "$1"
    else printf '  FAIL %s\n       expected to contain: %s\n       got: %s\n' "$1" "$2" "$3"; failures=$((failures+1)); fi
}
pg() { docker compose exec -T postgres psql -U postgres -d app -tAc "$1" 2>&1; }

run_connector() { # label db_class connector_name schema
    local label="$1" db_class="$2" connector="$3" schema="$4"
    local ss=(docker compose --profile silverstripe run --rm -T
              -e SS_SKIP_DEV_BUILD=1 -e "SS_DATABASE_NAME=$schema" -e "SS_DATABASE_CLASS=$db_class"
              silverstripe)

    printf '\n=== %s (%s -> schema %s) ===\n' "$label" "$db_class" "$schema"
    docker compose exec -T postgres psql -U postgres -d app -q \
        -c "DROP SCHEMA IF EXISTS $schema CASCADE" >/dev/null 2>&1

    local out
    out="$("${ss[@]}" vendor/bin/sake dev/build flush=1 2>&1)"
    check "dev/build completes"                 "Database build completed"   "$out"
    check "CMS tables created"                  "Table SiteTree: created"    "$out"
    check "example table created"               "Table ProofRecord: created" "$out"

    out="$("${ss[@]}" vendor/bin/sake dev/tasks/mysql2pg-driver-report flush=1 2>&1)"
    check "connector is $connector"             "connector=$connector"       "$out"
    check "server reports as MariaDB"           "MariaDB"                    "$out"
    check "ANSI quoting is in effect"           "ansi=yes"                   "$out"

    out="$("${ss[@]}" vendor/bin/sake dev/tasks/mysql2pg-proof flush=1 2>&1)"
    check "ORM round-trip"                      "total=3 active=2"  "$out"
    check "aggregate SUM"                       "sum_qty=20"        "$out"
    check "aggregate MAX"                       "max_price=101.25"  "$out"
    check "ORDER BY picks the right row"        "top=Gadget"        "$out"
    check "partial-match filter"                "partial=Gadget"    "$out"
    check "UPDATE is visible on re-read"        "updated=99"        "$out"

    check "CMS tables exist in PostgreSQL"      "SiteTree" \
          "$(pg "SELECT tablename FROM pg_tables WHERE schemaname = '$schema' AND tablename = 'SiteTree'")"
    check "row count matches"                   "3" \
          "$(pg "SELECT count(*) FROM $schema.\"ProofRecord\"")"
    check "updated value persisted"             "99" \
          "$(pg "SELECT \"Quantity\" FROM $schema.\"ProofRecord\" WHERE \"Title\" = 'Gadget'")"
    check "decimal survived the round trip"     "101.25" \
          "$(pg "SELECT \"Price\" FROM $schema.\"ProofRecord\" WHERE \"Title\" = 'Doohickey'")"
    check "the default home page was written"   "1" \
          "$(pg "SELECT count(*) FROM $schema.\"SiteTree\" WHERE \"URLSegment\" = 'home'")"
}

run_mysqli_protocol_probe() {
    printf '\n=== raw mysqli protocol (schema mysqli_probe) ===\n'
    docker compose exec -T postgres psql -U postgres -d app -q \
        -c "DROP SCHEMA IF EXISTS mysqli_probe CASCADE" >/dev/null 2>&1
    local out
    out="$(docker compose --profile silverstripe run --rm -T -e SS_SKIP_DEV_BUILD=1 \
              -e MW_DB=mysqli_probe silverstripe php /usr/local/bin/mysqli-probe.php 2>&1)"
    echo "$out" | grep -E "^  (ok|FAIL|connected)" || true
    if grep -q "FAIL" <<<"$out"; then
        failures=$((failures + $(grep -c "FAIL" <<<"$out")))
    fi
}

case "${1:-both}" in
    pdo)    run_connector "PDO"    MySQLPDODatabase PDOConnector    ss_pdo ;;
    mysqli) run_connector "mysqli" MySQLDatabase    MySQLiConnector ss_mysqli ;;
    probe)  run_mysqli_protocol_probe ;;
    both)
        run_connector "PDO"    MySQLPDODatabase PDOConnector    ss_pdo
        run_connector "mysqli" MySQLDatabase    MySQLiConnector ss_mysqli
        run_mysqli_protocol_probe
        ;;
    *) echo "usage: $0 [pdo|mysqli|probe|both]" >&2; exit 2 ;;
esac

echo
if (( failures )); then echo "$failures check(s) failed"; exit 1; fi
echo "all checks passed"
