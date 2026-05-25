# Matomo Test Setup

This repository includes an optional Docker Compose service for testing `mysql2pg-middleware` with a real MySQL-native application: Matomo.

## Version

The Compose service uses the official Matomo Docker image and defaults to `matomo:5.10.0-apache`, the current stable Apache tag checked on May 25, 2026.

You can override the tag without editing Compose:

```bash
MATOMO_IMAGE_TAG=5.10.0-apache docker compose --profile matomo up --build -d
```

## Start the stack

```bash
docker compose --profile matomo up --build -d
```

Matomo will be available on:

```text
http://127.0.0.1:8081
```

If `MATOMO_PORT` is set in `.env`, use that port instead.

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

## Configuration check

Validate the Matomo service without starting containers:

```bash
docker compose --profile matomo config
```

That confirms the Compose wiring and rendered image tag. It does not start Matomo or modify Matomo configuration.
