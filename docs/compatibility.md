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

### Proofs

Two applications are carried end to end, plus a synthetic suite:

| proof | what it covers |
|---|---|
| `tests/generic-app-smoke.sh` | a plain inventory schema over the MySQL protocol, no application involved |
| Matomo | see [matomo.md](matomo.md) — tracking and report archiving |
| `examples/silverstripe/` | SilverStripe CMS on its MariaDB driver over **both PDO and mysqli**, on **both the 4.13 and 5.x lines**; `smoke.sh` builds the schema, drives the ORM, then reads the rows back with `psql` |
| `examples/silverstripe/mysqli-probe.php` | raw mysqli against the middleware — text and binary protocol, `bind_result`, `store_result`, prepared `SHOW` |
| `examples/bookstack/` | BookStack (Laravel/Eloquent, MySQL-only upstream): all 104 migrations, then the query builder through write/read/aggregate/join/subquery |
| `examples/ghost/` | Ghost (Node/Knex/mysql2) — **incomplete**: boots and serves, but migration locking hits PostgreSQL's transaction-abort semantics. See its README |

SilverStripe is a deliberately different shape from Matomo: a different ORM, a
different quoting convention (`ANSI_QUOTES`), and heavy schema introspection.
Its framework and PHP versions are build arguments rather than pins, so the
proof is not tied to one release: 4.13 covers both client libraries because it
is the last line shipping PDO, and 5.x covers the current line on mysqli.

### Result column types

Announcing every column as a string is safe for PHP, where `"0"` is falsy, and
wrong for a client in a language where it is not: a Node client reading a
boolean column got the string `"0"`, which is truthy in JavaScript, and read
every such value backwards. Numeric columns are therefore announced with a
numeric type and written as numbers.

Temporal, decimal and binary columns are still announced as strings. They are
carried as text, and the protocol writer requires a value whose Rust type
matches the declared one, so announcing `DATETIME` without also encoding a date
fails the connection. Numbers are the case that changes client behaviour.

### The reported server version

Applications gate features on the server version, and they do not agree on what
they want: Matomo and SilverStripe expect MariaDB, Ghost refuses to run against
it and requires MySQL 8. The reported version is configurable with
`MW_MYSQL_SERVER_VERSION` (and `MW_MYSQL_SERVER_VERSION_COMMENT`), defaulting to
MariaDB.

### Client libraries

Both PHP MySQL clients are supported and tested, and they exercise the wire
protocol differently:

- **PDO** (`pdo_mysql`) with emulation off prepares nearly everything, so most
  statements arrive over the binary protocol.
- **mysqli** sends `query()` over the text protocol, and its `bind_result()`
  path depends on the column metadata the server returns at *prepare* time —
  so the column count and labels have to be right before any row is fetched.

Prepared `SHOW` statements matter here: clients such as Zend's mysqli adapter
prepare `SHOW VARIABLES LIKE ?`, `SHOW TABLES LIKE ?` and
`SHOW TABLE STATUS LIKE ?` with a bound parameter rather than inlining it, so
those forms accept a placeholder as well as a literal.

### Schema introspection

`SHOW FULL FIELDS` / `DESCRIBE` reconstruct a column's MySQL type from the
PostgreSQL catalog, because clients compare the reported type against the type
they declared. `integer` is reported as `int(11)`, `character varying(n)` as
`varchar(n)`, `numeric(p,s)` as `decimal(p,s)`, `timestamp` as `datetime`, and
defaults are stripped of their PostgreSQL casts (`'0'::smallint` becomes `0`).

This is a reconstruction, not a recording, so it cannot be exact where several
MySQL types collapse onto one PostgreSQL type: `enum(...)` comes back as
`mediumtext`, `tinyint(1) unsigned` as `smallint(6)`, and character set and
collation clauses are lost. Applications that reconcile their schema on every
startup may therefore keep trying to `ALTER` those columns. Recording the
declared type (for example in a column comment) and replaying it would close
this; see the limitation section in `examples/silverstripe/README.md`.

### Transactions

A MySQL client does not track transaction state itself: PDO's `inTransaction()`,
and its commit and rollback bookkeeping, read `SERVER_STATUS_IN_TRANS` out of
the server's OK packet. The middleware tracks the session's transaction state
and reports that flag, without which `beginTransaction()` appears to do nothing
and `commit()` fails with "There is no active transaction".

The self-healing repairs run under a savepoint when a transaction is open. A
failed statement aborts a PostgreSQL transaction, so without one the retry — and
the catalog lookup the repair needs — would both fail with 25P02.

### Implicit type coercion

MySQL coerces freely between text and numeric types, and a client that binds
every parameter as a string relies on it. PostgreSQL reports 42804 instead, so
on that error the executor wraps the INSERT's source in a derived table and casts
each column to its target type. That handles a `SELECT`, a `UNION` of them or
`VALUES` alike, which matters because a UNION's branches have to agree.

### `sql_mode` awareness

The session's `sql_mode` is tracked per connection. When it enables `ANSI` or
`ANSI_QUOTES`, double-quoted tokens are treated as identifiers instead of
string literals, matching MySQL. Clients that never set the mode are
unaffected. The value is picked up whether it is sent inline or bound as a
parameter of a prepared `SET sql_mode = ?`.

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
