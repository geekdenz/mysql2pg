# Compatibility Notes

This document tracks the MySQL/MariaDB interface currently implemented by `mysql2pg-middleware`.

## Statement coverage

### Query rewrites

Implemented today:

- MySQL backtick identifier normalization
- `LIMIT offset, count` -> `LIMIT count OFFSET offset`
- boolean literal normalization
- common function rewrites such as `IFNULL(...)`
- `JSON_EXTRACT(...)` rewrite for PostgreSQL execution

### DDL

Implemented today:

- `CREATE TABLE`

Supported inside `CREATE TABLE`:

- `IF NOT EXISTS`
- `AUTO_INCREMENT`
- integer, decimal, float, text, binary, JSON, enum, and datetime-family type mapping
- primary keys
- unique constraints
- foreign keys
- check constraints

Current behavioral compromises:

- unsigned MySQL types widen to PostgreSQL signed types where needed
- extra range `CHECK` constraints preserve unsigned bounds where practical
- `ENUM` becomes `TEXT` plus a `CHECK`
- `JSON` becomes `JSONB`
- `ON UPDATE CURRENT_TIMESTAMP` is not emulated with a trigger yet

Explicitly rejected today:

- MySQL inline `KEY` / `INDEX` definitions in `CREATE TABLE`
- `FULLTEXT`
- `SPATIAL`
- more complex table variants such as `CREATE TABLE ... LIKE`, `... AS SELECT`, or engine-specific features

### Metadata statements

Implemented today:

- `SHOW DATABASES`
- `SHOW SCHEMAS`
- `SHOW TABLES`
- `SHOW FULL TABLES`
- `SHOW VIEWS`
- `SHOW COLUMNS`
- `SHOW FULL COLUMNS`
- `DESC`
- `DESCRIBE`
- `SHOW CREATE TABLE`
- `SHOW CREATE VIEW`
- `SHOW VARIABLES`
- `SHOW STATUS`
- `SHOW COLLATION`
- `SHOW CHARSET`
- `SHOW FUNCTIONS`

Current limitations:

- `SHOW ... WHERE` support is incomplete
- `SHOW CREATE` only supports tables and views
- metadata result sets are compatibility-oriented, not byte-for-byte MySQL clones

## Type mapping summary

| MySQL / MariaDB | PostgreSQL |
| --- | --- |
| `TINYINT` | `SMALLINT` |
| `SMALLINT` | `SMALLINT` |
| `MEDIUMINT` | `INTEGER` |
| `INT` / `INTEGER` | `INTEGER` |
| `BIGINT` | `BIGINT` |
| `TINYINT UNSIGNED` | `SMALLINT` + range `CHECK` |
| `SMALLINT UNSIGNED` | `INTEGER` + range `CHECK` |
| `INT UNSIGNED` | `BIGINT` + range `CHECK` |
| `BIGINT UNSIGNED` | `BIGINT` with warning about full-range mismatch |
| `DECIMAL` / `NUMERIC` | `NUMERIC` |
| unsigned `DECIMAL` | `NUMERIC` + non-negative `CHECK` |
| `FLOAT` | `REAL` |
| `DOUBLE` | `DOUBLE PRECISION` |
| unsigned float/double | PostgreSQL float type + non-negative `CHECK` |
| `BOOLEAN` / `BOOL` | `BOOLEAN` |
| `CHAR` / `VARCHAR` / `TEXT` family | PostgreSQL text/string family |
| `TINYTEXT` / `MEDIUMTEXT` / `LONGTEXT` | `TEXT` |
| `BLOB` family | `BYTEA` |
| `BINARY` / `VARBINARY` | `BYTEA` |
| `JSON` | `JSONB` |
| `ENUM(...)` | `TEXT` + `CHECK` |
| `SET(...)` | `TEXT` |
| `DATETIME` | `TIMESTAMP` |
| `TIMESTAMP` | `TIMESTAMP` |

## Application independence

The middleware targets MySQL/MariaDB *dialect* compatibility, not any one
application. Nothing in the translation or execution path is keyed on a particular
schema or table name, with the single documented exception below.

### Dialect gaps handled generally

These are MySQL behaviors PostgreSQL does not share. Each is resolved from the
statement or the PostgreSQL catalog, so it works for any application:

- **Relaxed `GROUP BY`** — MySQL lets a `SELECT` list or `ORDER BY` reference a
  column that is neither grouped nor aggregated. Those expressions are wrapped in
  `MIN(...)`, including columns nested inside a `CASE`.
- **Identifier case folding** — MySQL returns a column under its alias's exact
  case; PostgreSQL folds unquoted identifiers to lower case. Aliases are quoted so
  the original spelling survives, and references are matched case-insensitively.
- **Comparisons as integers** — a comparison in a `SELECT` list yields `1`/`0` in
  MySQL and `boolean` in PostgreSQL (which has no `min()`), so those are cast to
  `int`, preserving `NULL`.
- **Implicit defaults** — MySQL accepts an `INSERT` that omits a `NOT NULL` column
  with no `DEFAULT`, storing `''` or `0`. On PostgreSQL's `23502` the column's type
  is read from the catalog and MySQL's implicit default supplied. Date columns are
  excluded: MySQL's `0000-00-00` has no PostgreSQL equivalent.
- **`ON DUPLICATE KEY UPDATE`** — becomes `ON CONFLICT ... DO UPDATE`. Which key a
  duplicate violates is a schema property the translator cannot see, so it guesses;
  if PostgreSQL rejects the guess (`42P10`) the table's real primary key is read
  from `pg_index` and the statement retried. `INSERT IGNORE` combined with the
  clause is accepted — the upsert governs the duplicate key.
- **Missing functions** — MySQL builtins with no PostgreSQL equivalent (`CRC32`,
  `SHA2`, `LOCATE`, `HOUR`, `HEX`, …) are created as PostgreSQL functions, eagerly
  at startup and just-in-time on first use. See `MYSQL_COMPAT_FUNCTIONS` in
  `src/executor.rs`.
- **Binary-protocol values** — over prepared statements PostgreSQL sends values in
  binary, so timestamp, date, time and numeric columns are decoded explicitly
  rather than surfacing as a placeholder string.
- **Error codes** — PostgreSQL `SQLSTATE`s map to the matching MySQL error numbers
  (e.g. `23505` → `1062 ER_DUP_ENTRY`), so client code that branches on a specific
  error behaves as it would on MySQL.
- **Session state** — `SELECT LAST_INSERT_ID()`, `CONNECTION_ID()`, `DATABASE()`
  and system variables are answered from per-connection state.

### The one application-shaped rewrite

`rewrite_mysql_ranking_query` recognises the top-N query Matomo's
`Piwik\RankingQuery` builds from MySQL user session variables (`@counter1 :=
@counter1 + 1`, cross-joined `( SELECT @counter:=0 ) initCounter` entries) and
rewrites it to `ROW_NUMBER()` window functions. It matches on that generated shape,
including the `initCounter`/`actualQuery` aliases, so it does nothing for other
applications. General MySQL session variables are **not** emulated; a statement
using `:=` in any other shape is passed through and will fail on PostgreSQL rather
than be silently mistranslated.

### Verifying with a non-Matomo workload

`tests/generic-app-smoke.sh` drives the middleware through a plain inventory schema
— DDL with MySQL types, auto-increment, `LAST_INSERT_ID()`, upserts, NULLs,
aggregates, relaxed `GROUP BY`, alias casing, `LIMIT offset,count`, transactions,
`SHOW TABLES`/`DESCRIBE`, and implicit defaults — with no Matomo tables involved:

```bash
docker compose up -d --build
./tests/generic-app-smoke.sh
```

## Verification workflow

The repository fixture [compatibility-suite.sql](/home/denz/projects/denz/mysql2pg-middleware/compatibility-suite.sql) is intended for end-to-end checks against the running middleware.

Typical local workflow:

```bash
docker compose up --build -d
mariadb -h 127.0.0.1 -P 3306 -u anyuser < compatibility-suite.sql
mariadb -h 127.0.0.1 -P 3306 -u anyuser -e "show full tables; desc qa_order_items; show create view qa_customer_totals;"
```

## Next obvious gaps

- `SHOW INDEX` / `SHOW KEYS`
- `SHOW TRIGGERS`
- `SHOW PROCEDURE STATUS`
- broader `ALTER TABLE`
- prepared statement parameter support
