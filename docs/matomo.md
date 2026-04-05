# Matomo Test Setup

This repository includes an optional Docker Compose service for testing `mysql2pg-middleware` with a real MySQL-native application: Matomo.

## Version

The Compose service is pinned to Matomo `5.8.0-apache`, which was the latest stable release verified for this repository on April 5, 2026.

## Start the stack

```bash
docker compose --profile matomo up --build -d
```

Matomo will be available on:

```text
http://127.0.0.1:8081
```

## Database wiring

Matomo talks to the middleware over the MySQL protocol:

- host: `middleware`
- adapter: `MYSQLI`
- database: `app`
- username: `anyuser`
- password: `matomo`

PostgreSQL is still the actual storage engine behind the middleware.

## What this verifies

This setup is useful for checking:

- client handshake compatibility
- metadata statement coverage such as `SHOW VARIABLES` and `SHOW TABLES`
- whether a real PHP application can reach the translated backend
- which remaining MySQL features block full installation

## Current expectation

The Matomo service should be able to start and reach the initial web installer. Full application installation may still hit unsupported SQL, especially around broader MySQL DDL such as `ALTER TABLE`, index management, and other schema-management statements not fully translated yet.

## Verified result

Verified in this repository on April 5, 2026:

- the Matomo `5.8.0-apache` container started successfully
- the installer page rendered with the title `Matomo 5.8.0 › Installation`
- Matomo's bundled PHP `mysqli` client connected to the middleware and successfully executed:
  - `SHOW VARIABLES LIKE 'version%'`
  - `SHOW TABLES`

That confirms the Compose wiring and the initial MySQL compatibility path for a real Matomo container. It does not yet guarantee that the full Matomo database installation workflow completes end to end.
