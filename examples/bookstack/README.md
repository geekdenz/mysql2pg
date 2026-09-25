# BookStack on PostgreSQL, through the middleware

A third application-independence proof, after the generic suite and
SilverStripe. BookStack is a good next step for two reasons: it supports
**MySQL/MariaDB only** upstream, so there is no PostgreSQL code path in the
application to quietly take instead, and it is a **Laravel** application, so
its SQL comes from Eloquent's MySQL grammar rather than a hand-written query
builder.

```
BookStack ──PDO / MySQL wire──▶ mysql2pg-middleware ──libpq──▶ PostgreSQL
  (Laravel's mysql driver; thinks it is talking to MariaDB 11.8)
```

## Running it

```bash
docker compose --profile bookstack up -d --build
./examples/bookstack/smoke.sh
```

The site is then on <http://localhost:8084>.

Versions are build arguments, not pins:

```bash
BOOKSTACK_VERSION=v24.05 BOOKSTACK_PHP_VERSION=8.2 \
  docker compose --profile bookstack build bookstack
```

It builds into its own `bookstack` PostgreSQL schema, which `smoke.sh` drops at
the start of each run, so the example is repeatable and does not disturb the
other examples in the same database.

## What it proves

1. **Migrations run.** This is the demanding part: BookStack's migration history
   is a long series of `CREATE TABLE` and `ALTER TABLE` statements written for
   MySQL, replayed in order.
2. **Eloquent round-trips.** `mysql2pg:proof` is an artisan command that creates
   a book, a chapter and three pages, then exercises `count`, `sum`, `max`,
   `orderBy`, a `LIKE` filter, an `UPDATE` with re-read, and a `JOIN` with
   `GROUP BY`.
3. **The data is really in PostgreSQL.** The last checks bypass BookStack
   entirely and read the rows back with `psql`.

## Middleware gaps this found

Fifteen, all of them general MySQL behaviour rather than BookStack quirks:

| gap | fix |
|---|---|
| transaction status flags | OK packets now report `SERVER_STATUS_IN_TRANS`; without it PDO's `inTransaction()` was always false, so `beginTransaction()` silently did nothing and `commit()` failed with "There is no active transaction" |
| repairs inside a transaction | each repairable statement runs under a savepoint, because the first failure aborts the transaction and every later statement — including the repair's own catalog lookup — then fails with 25P02 |
| implicit type coercion | on 42804 the INSERT's source is wrapped in a derived table and each column cast to its target type, which fixes every mismatched column at once and works for a UNION or VALUES source too |
| `information_schema.columns` | extended with MySQL's `column_type`/`extra`, which clients read to reconstruct a declared type |
| `DROP FOREIGN KEY` | `DROP CONSTRAINT` |
| `schema()` / `DATABASE()` | resolve to `current_schema()`, since schemas are presented as databases |
| booleans over the wire | render as `1`/`0`; `"false"` is truthy in PHP |
| named constraints | `CONSTRAINT ""name""` — the ident was rendered with its backticks, then quoted again |
| `ADD COLUMN ... NOT NULL` | supply MySQL's implicit default so existing rows backfill |
| `RENAME TABLE a TO b` | `ALTER TABLE a RENAME TO b`, one per pair |
| prepared-statement failures | the repair loop (implicit defaults, ON CONFLICT targets) now covers the prepared path, not just simple queries |
| `information_schema.statistics` | served from `pg_index`, with `PRIMARY` named as MySQL names it |
| `GROUP_CONCAT` | `string_agg`, carrying ORDER BY and SEPARATOR |
| `UPDATE t SET t.c = ...` | drop the qualifier PostgreSQL rejects |
| `DROP PRIMARY KEY` | `DROP CONSTRAINT "<table>_pkey"` |
| `INSERT ... SELECT` | the SELECT now gets the same rewrites a standalone SELECT does |
