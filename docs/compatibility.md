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
