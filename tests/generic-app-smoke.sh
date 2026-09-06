#!/usr/bin/env bash
# Exercises the middleware as a *generic* MySQL application would, with no Matomo
# schema or query shapes involved. Run it against a started stack:
#
#   docker compose up -d --build
#   ./tests/generic-app-smoke.sh
#
# Every check prints "ok" or "FAIL <detail>"; the script exits non-zero if any failed.
set -uo pipefail

HOST="${MW_HOST:-127.0.0.1}"
PORT="${MW_PORT:-3306}"
USER="${MW_USER:-anyuser}"
PASS="${MW_PASS:-matomo}"
DB="${MW_DB:-app}"

failures=0
q() { mysql -h"$HOST" -P"$PORT" -u"$USER" -p"$PASS" "$DB" --skip-ssl -N -e "$1" 2>&1 | grep -v Deprecated; }
check() { # name expected actual
    if [[ "$3" == *"$2"* ]]; then printf 'ok   %s\n' "$1"
    else printf 'FAIL %s\n     expected to contain: %s\n     got: %s\n' "$1" "$2" "$3"; failures=$((failures+1)); fi
}

q "DROP TABLE IF EXISTS inv_items" >/dev/null
q "CREATE TABLE inv_items (
     id INT NOT NULL AUTO_INCREMENT,
     sku VARCHAR(64) NOT NULL,
     title TEXT,
     price DECIMAL(10,2) NOT NULL,
     qty INT UNSIGNED NOT NULL,
     added_at DATETIME NOT NULL,
     active TINYINT(1) NOT NULL,
     PRIMARY KEY (id),
     UNIQUE KEY uniq_sku (sku)
   ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4" >/dev/null

q "INSERT INTO inv_items (sku, title, price, qty, added_at, active)
   VALUES ('SKU-1','Widget',9.99,5,'2026-09-06 10:00:00',1)" >/dev/null

check "select round-trips types"  "SKU-1	9.99	5	2026-09-06 10:00:00" \
      "$(q "SELECT sku, price, qty, added_at FROM inv_items")"
check "auto-increment + LAST_INSERT_ID()" "2" \
      "$(q "INSERT INTO inv_items (sku, price, qty, added_at, active) VALUES ('SKU-2',1.50,2,'2026-09-06 11:00:00',0); SELECT LAST_INSERT_ID()")"
check "ON DUPLICATE KEY UPDATE upsert" "19.99	7" \
      "$(q "INSERT INTO inv_items (id, sku, price, qty, added_at, active) VALUES (1,'SKU-1',19.99,7,'2026-09-06 12:00:00',1) ON DUPLICATE KEY UPDATE price=19.99, qty=7; SELECT price, qty FROM inv_items WHERE id=1")"
check "NULL stays NULL" "1" \
      "$(q "SELECT title IS NULL FROM inv_items WHERE sku='SKU-2'")"
check "aggregates" "2	9	10.75" \
      "$(q "SELECT COUNT(*), SUM(qty), ROUND(AVG(price),2) FROM inv_items")"
check "MySQL's relaxed GROUP BY" "SKU-1" \
      "$(q "SELECT active, sku, COUNT(*) AS c FROM inv_items GROUP BY active")"
check "alias casing preserved" "SKU-1" \
      "$(q "SELECT sku AS itemSku FROM inv_items ORDER BY itemSku LIMIT 1")"
check "MySQL functions" "SKU-2-x	none	11" \
      "$(q "SELECT CONCAT(sku,'-x'), IFNULL(title,'none'), HOUR(added_at) FROM inv_items WHERE sku='SKU-2'")"
check "LIMIT offset,count" "SKU-2" \
      "$(q "SELECT sku FROM inv_items ORDER BY id LIMIT 1, 1")"
check "UPDATE" "12" \
      "$(q "UPDATE inv_items SET qty = qty + 10 WHERE sku='SKU-2'; SELECT qty FROM inv_items WHERE sku='SKU-2'")"
check "DELETE" "1" \
      "$(q "DELETE FROM inv_items WHERE sku='SKU-2'; SELECT COUNT(*) FROM inv_items")"
check "SHOW TABLES" "inv_items" "$(q "SHOW TABLES LIKE 'inv_%'")"
check "DESCRIBE" "auto_increment" "$(q "DESCRIBE inv_items")"
check "transaction commits" "99" \
      "$(q "START TRANSACTION; UPDATE inv_items SET qty=99 WHERE id=1; COMMIT; SELECT qty FROM inv_items WHERE id=1")"

# MySQL supplies an implicit default for a NOT NULL column an INSERT omits;
# PostgreSQL rejects it, so the middleware fills it from the catalog.
q "DROP TABLE IF EXISTS implicit_defaults" >/dev/null
q "CREATE TABLE implicit_defaults (
     ref VARCHAR(20) NOT NULL PRIMARY KEY,
     note VARCHAR(50) NOT NULL,
     amount INT NOT NULL
   )" >/dev/null
q "INSERT INTO implicit_defaults (ref) VALUES ('A-1')" >/dev/null
check "implicit defaults for omitted NOT NULL columns" "A-1		0" \
      "$(q "SELECT ref, note, amount FROM implicit_defaults")"

# Binary columns must round-trip byte-for-byte. PostgreSQL renders a bytea as the
# text `\x…` over the simple protocol, which would hand the client an ASCII string
# twice the size instead of its data.
q "DROP TABLE IF EXISTS blob_payloads" >/dev/null
q "CREATE TABLE blob_payloads (id INT NOT NULL PRIMARY KEY, payload BLOB NOT NULL)" >/dev/null
q "INSERT INTO blob_payloads (id, payload) VALUES (1, UNHEX('89504E470D0A1A0A0000000D49484452'))" >/dev/null
check "binary round-trips without hex escaping" "89504E470D0A1A0A0000000D49484452" \
      "$(q "SELECT HEX(payload) FROM blob_payloads WHERE id = 1")"
check "binary length preserved" "16" \
      "$(q "SELECT LENGTH(payload) FROM blob_payloads WHERE id = 1")"

q "DROP TABLE IF EXISTS inv_items" >/dev/null
q "DROP TABLE IF EXISTS implicit_defaults" >/dev/null
q "DROP TABLE IF EXISTS blob_payloads" >/dev/null

if (( failures )); then printf '\n%d check(s) failed\n' "$failures"; exit 1; fi
printf '\nall checks passed\n'
