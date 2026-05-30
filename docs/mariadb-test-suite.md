# MariaDB Test Suite Harness

This repository now includes a repo-local compatibility suite in MariaDB/MySQL server-test layout under [mysql-test/suite/mysql2pg](/home/denz/projects/denz/mysql2pg-middleware/mysql-test/suite/mysql2pg).

It is designed as the first broad compatibility filter against the middleware's MySQL-compatible frontend:

- SQL semantics
- result-set formatting
- metadata shape surprises
- session/state regressions that show up in normal client/server use
- text protocol coverage

It is not sufficient on its own for connector compatibility. In particular, it does not prove that every client library's prepared statement or binary protocol behavior matches connector expectations.

## Layout

- tests: [mysql-test/suite/mysql2pg/t](/home/denz/projects/denz/mysql2pg-middleware/mysql-test/suite/mysql2pg/t)
- expected results: [mysql-test/suite/mysql2pg/r](/home/denz/projects/denz/mysql2pg-middleware/mysql-test/suite/mysql2pg/r)
- runner: [scripts/run-mariadb-suite.sh](/home/denz/projects/denz/mysql2pg-middleware/scripts/run-mariadb-suite.sh)

The suite uses the locally installed `mariadb-test` client. We do not currently have `mysql-test-run.pl` installed in this environment, so the runner executes each `.test` file directly while preserving the standard suite layout.

## Current tests

- `text_smoke`: DDL, DML, boolean handling, ordering, aggregate formatting
- `metadata`: `SHOW FULL TABLES`, `DESC`, `SHOW CREATE VIEW`
- `json_functions`: `JSON_EXTRACT(...)` and `IFNULL(...)` translation
Prepared-statement protocol coverage is intentionally not baselined in this suite yet. It needs a separate connector-focused lane because a broad SQL/resultset filter should not normalize binary-protocol corruption or metadata bugs into expected results.

## Running

Start the adapter first, for example:

```bash
docker compose up --build -d postgres middleware
```

Then verify against the stored expected results:

```bash
scripts/run-mariadb-suite.sh
```

To re-record expected results after an intentional compatibility change:

```bash
scripts/run-mariadb-suite.sh --record
```

To run only selected tests:

```bash
scripts/run-mariadb-suite.sh text_smoke metadata
```

## Environment knobs

- `MARIADB_TEST_BIN`
- `MYSQL_TEST_HOST`
- `MYSQL_TEST_PORT`
- `MYSQL_TEST_USER`
- `MYSQL_TEST_PASSWORD`
- `MYSQL_TEST_DATABASE`
- `MYSQL_TEST_LOGDIR`

## Why this exists

The existing Rust unit/smoke tests are useful for translation correctness, but they are too narrow to act as the first compatibility gate. This suite is intentionally closer to the MySQL/MariaDB server test model and is meant to catch broader SQL- and result-shape regressions before connector-specific testing starts.
