# SilverStripe on PostgreSQL, through the middleware

A second application-independence proof, alongside `tests/generic-app-smoke.sh`.
Matomo shows the middleware can carry one demanding application; this shows the
same binary carrying a completely unrelated one, with a different ORM, a
different dialect habit, and no shared code.

Nothing in the container knows PostgreSQL exists. SilverStripe is configured
with its own stock MariaDB driver and talks the MySQL wire protocol; the
middleware translates, and PostgreSQL stores.

Both of PHP's MySQL client libraries are covered, because they use the wire
protocol differently: **PDO** (`pdo_mysql`) and **mysqli**. With prepared
statements unemulated, PDO leans on the binary protocol, while mysqli's
`query()` uses the text protocol and its `bind_result()` path depends on the
column metadata returned at prepare time.

```
SilverStripe ──PDO / MySQL wire──▶ mysql2pg-middleware ──libpq──▶ PostgreSQL
  (thinks it is talking to MariaDB 11.8)
```

## Running it

```bash
docker compose --profile silverstripe  up -d --build   # 4.13: PDO + mysqli
docker compose --profile silverstripe5 up -d --build   # 5.x:  mysqli only

./examples/silverstripe/smoke.sh          # 4.13 both connectors + protocol probe
./examples/silverstripe/smoke.sh all      # the above plus SilverStripe 5
./examples/silverstripe/smoke.sh mysqli   # one of: pdo|mysqli|probe|ss5|both|all
```

## Choosing versions

Nothing is hard-pinned. The framework and PHP versions are build arguments on a
single Dockerfile, so the example tracks whatever line you want:

| profile | default | connectors | port |
|---|---|---|---|
| `silverstripe`  | 4.13 on PHP 8.1 | PDO **and** mysqli | 8082 |
| `silverstripe5` | 5.4 on PHP 8.3  | mysqli only        | 8083 |

Override per profile with `SILVERSTRIPE_VERSION` / `SILVERSTRIPE_PHP_VERSION`
and `SILVERSTRIPE5_VERSION` / `SILVERSTRIPE5_PHP_VERSION`, for example:

```bash
SILVERSTRIPE5_VERSION=^5.2 SILVERSTRIPE5_PHP_VERSION=8.2 \
  docker compose --profile silverstripe5 build silverstripe5
```

Both lines pass the same 17 checks, and SilverStripe 5 needed no middleware
changes beyond what 4.13 already required.

The CMS is then on <http://localhost:8082> (or 8083 for the 5.x profile), with
admin / password by default. On 4.13, set `SS_DATABASE_CLASS=MySQLDatabase` to
run the site itself on mysqli instead of PDO.

Each connector builds into its own PostgreSQL schema (`ss_pdo`, `ss_mysqli`),
dropped at the start of every run, so the two are verified independently.

`mysqli-probe.php` additionally talks to the middleware with raw mysqli, no ORM
in the way, so a protocol regression is attributed clearly rather than showing
up as a puzzling ORM failure.

## Why both lines

**Why 4.13 is still here alongside 5.x.** SilverStripe 5 removed the PDO
connector, so 4.13 is the only line that ships *both*. Keeping it is what makes
the two-connector comparison possible from one image: the application, its
version and its schema are identical, so any difference is attributable to the
client library alone. 5.x then shows the current release working on the same
middleware. Neither is load-bearing for the other.

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
