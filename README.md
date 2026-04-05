# mysql2pg-middleware

A Rust middleware that:

1. Parses incoming SQL with the MySQL dialect.
2. Produces a canonical SQL string from the parsed AST.
3. Applies PostgreSQL-oriented translation passes.
4. Connects to PostgreSQL through a configurable executor layer.
5. Can run as either a CLI or an HTTP service.

## What is implemented in this iteration

- MySQL parsing with `sqlparser`
- PostgreSQL execution abstraction with a configurable driver key
- `tokio-postgres` driver implementation
- HTTP middleware service with these endpoints:
  - `GET /health`
  - `POST /translate`
  - `POST /execute`
- Docker Compose setup with a sample `.env` file
- Translation passes for common MySQL → PostgreSQL differences:
  - backtick identifiers to PostgreSQL double quotes
  - `LIMIT offset, count` → `LIMIT count OFFSET offset`
  - `IFNULL()` → `COALESCE()`
  - `RAND()` → `RANDOM()`
  - `UNIX_TIMESTAMP(expr)` → `EXTRACT(EPOCH FROM expr)`
  - `FROM_UNIXTIME(expr)` → `TO_TIMESTAMP(expr)`
  - selected `JSON_EXTRACT(col, '$.a.b')` → `col #>> '{a,b}'`
  - stripping `ENGINE=` / `CHARSET=` table options in simple DDL cases
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
- `src/server.rs` — HTTP service endpoints
- `src/config.rs` — TOML + environment-based config loading
- `src/main.rs` — CLI entrypoint
- `docker-compose.yml` — local stack for middleware + PostgreSQL
- `.env.example` — sample environment file

## Local CLI usage

### Translate SQL

```bash
cargo run -- --config config/example.toml translate --sql "SELECT `id`, IFNULL(name, 'x') FROM `users` LIMIT 5, 10"
```

### Execute translated SQL

```bash
cargo run -- --config config/example.toml execute --sql "SELECT `id`, IFNULL(name, 'x') FROM `users` LIMIT 5, 10"
```

### Run the HTTP service locally

```bash
cargo run -- --config config/example.toml serve
```

## Docker Compose usage

1. Copy the sample env file:

```bash
cp .env.example .env
```

2. Build and start the stack:

```bash
docker compose up --build
```

3. Test the middleware:

```bash
curl http://localhost:8080/health
```

```bash
curl -X POST http://localhost:8080/translate \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT `id`, IFNULL(name, '\''x'\'') FROM `users` LIMIT 5, 10"}'
```

```bash
curl -X POST http://localhost:8080/execute \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT 1 LIMIT 0, 1"}'
```

## Environment variables

These environment variables override values from the TOML config file:

- `MW_SERVER_BIND_ADDR`
- `MW_POSTGRES_DRIVER`
- `MW_POSTGRES_CONNECTION_STRING`
- `MW_TRANSLATOR_REWRITE_LIMIT_COMMA`
- `MW_TRANSLATOR_NORMALIZE_MYSQL_BACKTICKS`
- `MW_TRANSLATOR_NORMALIZE_BOOLEAN_LITERALS`
- `MW_TRANSLATOR_REWRITE_MYSQL_FUNCTIONS`
- `MW_TRANSLATOR_REWRITE_JSON_OPERATORS`
- `MW_TRANSLATOR_STRIP_MYSQL_TABLE_OPTIONS`

## Notes

- The Docker image starts the binary in `serve` mode.
- The middleware still uses a translation-first, fail-fast strategy for unsupported MySQL features.
- This environment does not have a Rust toolchain or Docker daemon available, so I could not build-verify the project here.

## Next steps

- Add prepared statement and bind parameter support.
- Add schema-aware translation for `AUTO_INCREMENT` and `ON DUPLICATE KEY UPDATE`.
- Add more driver backends behind the executor trait.
- Add authentication, request logging, and metrics to the HTTP service.


## Notes

- JSON and JSONB result rendering is enabled through the `tokio-postgres` feature `with-serde_json-1`.
- BYTEA values are rendered as PostgreSQL-style hex strings.
