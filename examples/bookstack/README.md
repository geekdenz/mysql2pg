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

## Status: incomplete

**BookStack's migrations do not yet run to completion.** They get about 70 of
roughly 100 migrations in, which is far enough to create the whole entity schema
but not far enough to boot the application, so `smoke.sh` currently fails at its
first check. The example is committed because each step of that progress came
from a real, general middleware fix (see below), and because the remaining
blocker is a single well-understood class of problem.

**What is left: MySQL's implicit type coercion.** The migrations reach
statements like

```sql
insert into entity_permissions (entity_id, role_id, ...) select ... , 'text' ...
```

where MySQL silently coerces a text expression into a `bigint` column and
PostgreSQL refuses with 42804. Fixing it properly means casting the expression
to the target column's type, which needs the column types — so it belongs in the
executor, as a repair on 42804 alongside the existing 23502 and 42P10 repairs,
rather than in the translator, which cannot see the catalog.

## What it will prove

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

Nine, all of them general MySQL behaviour rather than BookStack quirks:

| gap | fix |
|---|---|
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
