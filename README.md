# mysql2pg-middleware

A Rust middleware that:

1. Parses incoming SQL with the MySQL dialect.
2. Produces a canonical SQL string from the parsed AST.
3. Applies PostgreSQL-oriented translation passes.
4. Connects to PostgreSQL through a configurable executor layer.
5. Exposes both an HTTP API and a MySQL-compatible TCP frontend.

## What changed in this iteration

- Added a MySQL-compatible wire-protocol frontend using `opensrv-mysql`.
- The `serve` command now starts:
  - HTTP on `0.0.0.0:8080`
  - MySQL-compatible TCP on `0.0.0.0:3306`
- Updated the Docker builder image from Rust `1.86` to Rust `1.94.1` so crates that rely on stabilized let-chains can build.

## Important current limitations

- The MySQL frontend currently translates and executes text queries against PostgreSQL.
- Prepared statements are only supported when they have **no bind parameters**.
- Prepared statements with `?` parameters return a clear "not implemented yet" MySQL error.
- Result-set columns are currently exposed to MySQL clients as string-like columns for compatibility, even when PostgreSQL returned numeric types.
- Authentication is permissive in this iteration so local/docker testing is easy.

## Docker Compose usage

```bash
cp .env.example .env
docker compose up --build -d
```

### Test the HTTP API

```bash
curl http://localhost:8080/health
```

### Connect with a MySQL client

```bash
mysql -h 127.0.0.1 -P 3306 -u anyuser -e "SELECT 1"
```

If you want to avoid colliding with a local MySQL server, set:

```env
MYSQL_FRONTEND_PORT=3307
```

## Notes

I could not compile or run Docker in this environment, so this is a source-level update rather than a build-verified one.
