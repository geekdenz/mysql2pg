# Ghost on PostgreSQL, through the middleware

Ghost is the first **non-PHP** proof here: Node, Knex, and the `mysql2` driver,
so it exercises a second independent implementation of the MySQL wire protocol.
It is also MySQL-only upstream — Ghost dropped every other database — so there
is no PostgreSQL path in the application to fall back on.

```bash
MW_MYSQL_SERVER_VERSION=8.0.35 docker compose --profile ghost up -d --build
```

The site would be on <http://localhost:8085>.

`MW_MYSQL_SERVER_VERSION` is required: Ghost checks the server version and
refuses to run against MariaDB, while Matomo and SilverStripe expect MariaDB.
The reported version is therefore configurable per deployment rather than fixed.

## Status: incomplete

Ghost **boots, connects, and serves HTTP**, and its first tables are created. It
does not finish migrating: `knex-migrator` cannot acquire its migration lock.

The cause is a semantic difference that is not about SQL syntax at all:

> On MySQL a failed statement leaves the surrounding transaction usable. On
> PostgreSQL it aborts the transaction, and every subsequent statement fails
> with 25P02 until the transaction is unwound.

Knex depends on the MySQL behaviour. It creates `migrations_lock` **with** a
primary key and then issues `ALTER TABLE migrations_lock ADD PRIMARY KEY`
anyway, expecting the duplicate to fail harmlessly. On PostgreSQL that failure
poisons the transaction it happens in, and the lock handling never recovers.

Work already done towards this:

- Failed statements inside a transaction run under a savepoint, so the
  transaction survives them (`begin_repair_savepoint` in `src/executor.rs`).
- `SET search_path` is cached and no longer re-issued per statement, and a
  failure to apply it no longer masks the error that aborted the transaction.
- `42P16` and the other DDL SQLSTATEs now map to the MySQL error codes clients
  branch on, so a client can recognise "already exists" and carry on.

What remains is knowing reliably when a transaction is open. The middleware
tracks `BEGIN`/`COMMIT`/`ROLLBACK` as it sees them, but the statement that
aborts Ghost's transaction is observed with no transaction open, so no savepoint
is taken — the tracking and PostgreSQL's real state disagree somewhere in
Knex's connection handling. Until that is resolved the savepoint protection has
a hole, and this example stays red.

## What it did prove

The route to Ghost surfaced two gaps that were real and are fixed:

| gap | why it mattered |
|---|---|
| result columns were all announced as strings | `"0"` is falsy in PHP but **truthy in JavaScript**, so a Node client read every boolean backwards. Numeric columns are now announced as numbers |
| the server version was a hardcoded MariaDB string | applications gate features on it, and they do not agree on what they want |
