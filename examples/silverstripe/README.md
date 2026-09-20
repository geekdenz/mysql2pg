# SilverStripe on PostgreSQL, through the middleware

A second application-independence proof, alongside `tests/generic-app-smoke.sh`.
Matomo shows the middleware can carry one demanding application; this shows the
same binary carrying a completely unrelated one, with a different ORM, a
different dialect habit, and no shared code.

Nothing in the container knows PostgreSQL exists. SilverStripe is configured
with its own stock MariaDB driver over PDO and talks the MySQL wire protocol;
the middleware translates, and PostgreSQL stores.

```
SilverStripe ──PDO / MySQL wire──▶ mysql2pg-middleware ──libpq──▶ PostgreSQL
  (thinks it is talking to MariaDB 11.8)
```

## Running it

```bash
docker compose --profile silverstripe up -d --build
./examples/silverstripe/smoke.sh
```

The CMS is then on <http://localhost:8082> (admin / password by default).

## Why these version pins

**SilverStripe 4.13, not 5.** SilverStripe 5 removed the PDO connector and
supports only `mysqli`. Since the point here is to exercise a *PDO* client,
this example pins the last release line that ships `MySQLPDODatabase`. That
also fixes PHP at 8.1, which is what 4.13 supports.

## What it actually proves

`smoke.sh` checks three separate things, in increasing order of how hard they
are to fake:

1. **The schema builds.** `dev/build` is the harshest test in the suite:
   SilverStripe introspects every table it manages with `SHOW FULL FIELDS` and
   `SHOW INDEXES`, then issues `CREATE TABLE` / `ALTER TABLE` to reconcile them.
2. **The driver is what we claim.** `dev/tasks/mysql2pg-driver-report` asks the
   *live connection* which connector class it is using and what `VERSION()` the
   server reports, so the proof cannot drift out of sync with the config.
3. **The data is really in PostgreSQL.** The last checks bypass SilverStripe
   entirely and read the rows back with `psql`. Writes go in through the MySQL
   protocol and come out of a PostgreSQL table.

## Known limitation: re-running `dev/build`

A first `dev/build` against an empty schema is clean: 40 tables created, no
`ALTER`s, exit 0. Running it *again* against the schema it just built is not —
SilverStripe re-reads the columns, decides 142 of them differ from what it
asked for, and emits `ALTER TABLE ... CHANGE` statements that the translator
rejects (`UNSIGNED`/`AUTO_INCREMENT` in DDL are not implemented).

The cause is not the `ALTER`s, it is introspection fidelity. `SHOW FULL FIELDS`
reconstructs a column's type from the PostgreSQL catalog, and some MySQL types
have no distinguishable PostgreSQL counterpart to reconstruct from:

| declared | stored as | reported back | result |
|---|---|---|---|
| `enum('a','b')` | `text` | `mediumtext` | 45 columns |
| `varchar(255) character set utf8mb4 collate ...` | `varchar(255)` | `varchar(255)` | 43 columns |
| `tinyint(1) unsigned` | `smallint` | `smallint(6)` | 26 columns |

The middleware already maps the common types back (`integer` -> `int(11)`,
`character varying(n)` -> `varchar(n)`, `numeric` -> `decimal`), which is what
took this from 321 mismatches to 142. Closing the rest needs the *declared*
MySQL type to be remembered rather than inferred - for example stored as a
PostgreSQL column comment at `CREATE`/`ALTER` time and replayed by
`SHOW FULL FIELDS`. That would fix all 142 at once, and would help any
application that introspects its own schema, not just SilverStripe.

So: the example is a genuine proof that the CMS installs and runs, and it is
reproducible from scratch. It is not yet a claim that SilverStripe's schema
migrations are idempotent.

## The middleware changes this required

Six gaps surfaced, all of them general MySQL behaviour rather than anything
SilverStripe-specific:

1. **`ANSI_QUOTES`.** SilverStripe sets `sql_mode = 'ANSI'` on connect, and its
   own source says the mode "must always include ANSI or ANSI_QUOTES". Under
   that mode a double-quoted token is an *identifier*, not a string literal -
   the opposite of MySQL's default, which is what the middleware had assumed.
   The translator now tracks `sql_mode` per session. Note that SilverStripe
   sets it with a *prepared statement* (`SET sql_mode = ?`), so the value has
   to be read from the bound parameter, not the statement text - and it arrives
   binary-encoded rather than as text.
2. **`USE <db>`**, the statement form of `COM_INIT_DB`, now selects the schema.
3. **`SHOW FULL TABLES WHERE ...`**, which phpMyAdmin also uses.
4. **`SHOW TABLE STATUS`**, answered from `pg_class`.
5. **`SHOW INDEXES IN`** - only `FROM` was accepted before.
6. **Result column labels.** Over the prepared-statement path a column came
   back labelled `"SiteTree"."ClassName"` instead of `ClassName`, because the
   already-translated PostgreSQL SQL was being re-parsed with the MySQL
   dialect. MySQL labels a column with its bare name, never the qualifier.
