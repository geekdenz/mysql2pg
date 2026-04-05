# mysql2pg-middleware

A first-iteration Rust middleware/CLI that:

1. Parses incoming SQL with the MySQL dialect.
2. Produces a canonical SQL string from the parsed AST.
3. Applies PostgreSQL-oriented translation passes.
4. Connects to PostgreSQL through a configurable executor layer.
5. Executes the translated SQL when requested.

## What is implemented in this first iteration

- MySQL parsing with `sqlparser`
- PostgreSQL execution abstraction with a configurable driver key
- `tokio-postgres` driver implementation
- Translation passes for common MySQL → PostgreSQL differences:
  - backtick identifiers to PostgreSQL double quotes
  - `LIMIT offset, count` → `LIMIT count OFFSET offset`
  - `IFNULL()` → `COALESCE()`
  - `RAND()` → `RANDOM()`
  - `UNIX_TIMESTAMP(expr)` → `EXTRACT(EPOCH FROM expr)`
  - `FROM_UNIXTIME(expr)` → `TO_TIMESTAMP(expr)`
  - selected `JSON_EXTRACT(col, '$.a.b')` → `col #>> '{a,b}'`
  - stripping `ENGINE=`/`CHARSET=` table options in simple DDL cases
- Fail-fast detection for unsupported or schema-sensitive constructs:
  - `ON DUPLICATE KEY UPDATE`
  - `REPLACE INTO`
  - `AUTO_INCREMENT`
  - `UNSIGNED`
  - `STRAIGHT_JOIN`
  - `SQL_CALC_FOUND_ROWS`

## Project layout

- `src/parser.rs` — MySQL parsing
- `src/translator.rs` — translation pipeline and rules
- `src/executor.rs` — pluggable PostgreSQL executor trait + tokio-postgres adapter
- `src/config.rs` — TOML config model
- `src/main.rs` — CLI entrypoint
- `tests/` — smoke tests for core translation behavior

## Usage

### 1) Configure PostgreSQL

Edit `config/example.toml`:

```toml
[postgres]
driver = "tokio-postgres"
connection_string = "host=127.0.0.1 port=5432 user=postgres password=postgres dbname=postgres"
```

### 2) Translate SQL

```bash
cargo run -- --config config/example.toml translate --sql "SELECT `id`, IFNULL(name, 'x') FROM `users` LIMIT 5, 10"
```

### 3) Execute translated SQL

```bash
cargo run -- --config config/example.toml execute --sql "SELECT `id`, IFNULL(name, 'x') FROM `users` LIMIT 5, 10"
```

### 4) JSON output

```bash
cargo run -- --config config/example.toml translate --sql "SELECT * FROM users LIMIT 0, 5" --json
```

## Design notes

This version intentionally prioritizes:

- safe failure on unsupported constructs,
- a clean executor abstraction,
- easy extension of translation passes,
- an MVP that can evolve into an actual network middleware/proxy.

## Recommended next steps

- Add parameter-aware translation and prepared statement support.
- Add schema-aware DDL translation (`AUTO_INCREMENT` → `GENERATED ... AS IDENTITY`).
- Add translation for `ON DUPLICATE KEY UPDATE` to `ON CONFLICT` with table metadata.
- Add a TCP/HTTP service wrapper to expose this as a long-running middleware process.
- Add another executor backend, for example `sqlx`.
